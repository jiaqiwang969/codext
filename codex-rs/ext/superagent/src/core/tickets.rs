//! Tickets: their lifecycle from an accept to their end, their Subscriptions and their Inputs
//! (prototype `src/core/tickets.ts`).

use std::collections::HashMap;

use super::engine::Cx;
use super::engine::refuse;
use super::model::Assignment;
use super::model::AssignmentState;
use super::model::CloseOutcome;
use super::model::Closed;
use super::model::DecisionKind;
use super::model::Ended;
use super::model::Evaluation;
use super::model::Execution;
use super::model::Follow;
use super::model::Mandate;
use super::model::Origin;
use super::model::PolicyEffect;
use super::model::ReportState;
use super::model::ThreadRef;
use super::model::Ticket;
use super::model::TicketEndBy;
use super::model::TicketInput;
use super::model::TicketOutcome;
use super::model::Trigger;
use super::model::TriggerDisposition;
use super::model::latest_assignment;
use super::model::live_ticket;
use super::model::read_until;
use super::model::running_assignment;
use super::model::settled;
use super::model::thread_key;
use super::model::waits_for_person;
use super::ports::AcceptOutcome;
use super::ports::DomainEvent;
use super::replies;
use super::stage::can_retry;
use crate::ids::key;

const STATUS_REQUEST: &str = "Report the current ticket status with superagent.report. This is a status-only request: do not resume implementation or perform external actions.";

/// Whether an Input is a message no person wrote: CI's, a bot's, or the PM's note.
pub(crate) fn automatic(input: Option<&TicketInput>) -> bool {
    match input {
        Some(TicketInput::Note { .. }) => true,
        Some(TicketInput::Trigger { human, .. }) => *human != Some(true),
        _ => false,
    }
}

/// Whether a ticket's worker read a Trigger Input before `until`, its latest report or turn end.
fn read_before(input: &TicketInput, until: &str) -> bool {
    match input {
        TicketInput::Trigger {
            at,
            read_at,
            delivered_at,
            ..
        } => match read_at {
            None => {
                let reached = match delivered_at {
                    Some(delivered) if delivered > at => delivered.as_str(),
                    _ => at.as_str(),
                };
                reached <= until
            }
            Some(None) => false,
            Some(Some(read)) => read.as_str() < until,
        },
        _ => false,
    }
}

pub(crate) fn close_outcome(outcome: CloseOutcome) -> TicketOutcome {
    match outcome {
        CloseOutcome::Merged => TicketOutcome::Merged,
        CloseOutcome::Closed => TicketOutcome::Closed,
    }
}

fn outcome_name(outcome: TicketOutcome) -> &'static str {
    match outcome {
        TicketOutcome::Completed => "completed",
        TicketOutcome::Merged => "merged",
        TicketOutcome::Closed => "closed",
        TicketOutcome::Dropped => "dropped",
        TicketOutcome::Stopped => "stopped",
    }
}

/// A ticket's short name from its requested name or its title: lowercase ascii words joined by `_`.
fn short_name(outcome: &AcceptOutcome) -> String {
    let source = outcome.name.as_deref().unwrap_or(&outcome.title);
    let mut name = String::new();
    let mut last_underscore = true;
    for c in source.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            name.push(c);
            last_underscore = false;
        } else if !last_underscore {
            name.push('_');
            last_underscore = true;
        }
        if name.len() >= 24 {
            break;
        }
    }
    let name = name.trim_matches('_').to_owned();
    if name.is_empty() {
        "task".to_owned()
    } else {
        name
    }
}

/// The next display name of the PM's tickets: `tNNN_short_name`, numbered in kv.
async fn display_name(cx: &mut Cx<'_>, outcome: &AcceptOutcome) -> anyhow::Result<String> {
    let counter_key = format!("pm-counter:{}", cx.e.pm);
    let next = cx.tx.kv_get::<i64>(&counter_key).await?.unwrap_or(0) + 1;
    cx.tx.kv_set(&counter_key, &next).await?;
    Ok(format!("t{next:03}_{}", short_name(outcome)))
}

/// Creates the ticket an accept decided, with the channels and Threads of the Eval's Triggers, its
/// first Assignment and the acknowledgement the accept carries, and queues its launch.
pub(crate) async fn create(
    cx: &mut Cx<'_>,
    id: &str,
    decision_id: &str,
    mandate: Mandate,
    evaluation: &Evaluation,
    outcome: &AcceptOutcome,
    at: &str,
) -> anyhow::Result<()> {
    let trigger_ids = &evaluation.trigger_ids;
    // An Eval recorded before `primary` existed falls back to its last Trigger.
    let primary_id = evaluation
        .primary
        .clone()
        .or_else(|| trigger_ids.last().cloned());
    let primary = match &primary_id {
        Some(id) => cx.tx.trigger(id).await?,
        None => None,
    };
    let origin = Origin {
        trigger_id: primary
            .as_ref()
            .map(|t| t.id.clone())
            .or_else(|| trigger_ids.first().cloned())
            .unwrap_or_default(),
        trigger_ids: trigger_ids.to_vec(),
    };
    let mut ticket = Ticket {
        id: id.to_owned(),
        title: outcome.title.clone(),
        decision_id: decision_id.to_owned(),
        mandate,
        channels: Vec::new(),
        ended: None,
        attention: None,
        assignments: Vec::new(),
        inputs: Vec::new(),
        undelivered: Vec::new(),
        follows: None,
        answers: None,
        created_at: at.to_owned(),
        updated_at: at.to_owned(),
        last_activity: None,
        origin,
        execution: None,
        delivery: None,
    };
    for trigger_id in trigger_ids {
        if let Some(channel) = cx.tx.trigger(trigger_id).await?.and_then(|t| t.channel)
            && !ticket.channels.contains(&channel)
        {
            ticket.channels.push(channel);
        }
    }
    let name = display_name(cx, outcome).await?;
    ticket.execution = Some(Execution {
        name: name.clone(),
        directory: String::new(),
        assignment: outcome.note.clone().unwrap_or_default(),
    });
    cx.tx.kv_set(&format!("ticket-name:{name}"), &id).await?;
    if let Some(prepare) = cx.e.prepare_ticket() {
        ticket = prepare(ticket, outcome)?;
    }
    let assignment = new_assignment(&cx.now, &mut ticket);
    cx.tx.save_ticket(&ticket).await?;
    for trigger_id in trigger_ids {
        if let Some(trigger) = cx.tx.trigger(trigger_id).await? {
            for reference in &trigger.threads {
                // A message that named another ticket (handed back, then accepted as new work)
                // does not make that ticket's own Thread this ticket's: what names a ticket
                // reaches that ticket alone.
                if super::thread_rules::own_ticket_of(reference).is_some_and(|own| own != id) {
                    continue;
                }
                subscribe(cx, id, reference).await?;
            }
        }
    }
    // Its own Thread (rules R1 and R2): what names the ticket, or is said to its worker, reaches it.
    subscribe(cx, id, &super::thread_rules::ticket_thread(id)).await?;
    if let (Some(text), Some(primary)) = (&outcome.acknowledgement, &primary) {
        replies::acknowledge_accepted(cx, id, primary, text).await?;
    }
    let ticket = cx.require(id).await?;
    cx.executor_start(&ticket, &assignment).await?;
    cx.emit(DomainEvent::TicketCreated {
        ticket_id: id.to_owned(),
        decision_id: decision_id.to_owned(),
        strength: ticket.mandate.strength,
    });
    Ok(())
}

/// Subscribes an open ticket to a Thread and records it in `follows`, `own` when the ticket opened it.
pub(crate) async fn bind(
    cx: &mut Cx<'_>,
    ticket_id: &str,
    thread: &ThreadRef,
    own: bool,
) -> anyhow::Result<Ticket> {
    if !live_ticket(&cx.require(ticket_id).await?) {
        return Err(refuse("ticketEnded", &[]));
    }
    subscribe(cx, ticket_id, thread).await?;
    let mut ticket = cx.require(ticket_id).await?;
    let followed = thread_key(thread);
    let follows = ticket.follows.get_or_insert_with(Vec::new);
    match follows.iter_mut().find(|item| item.thread == followed) {
        // Binding a Thread again reopens it; `own` stays once set.
        Some(follow) => {
            follow.own = follow.own || own;
            follow.closed = None;
        }
        None => follows.push(Follow {
            thread: followed,
            own,
            closed: None,
        }),
    }
    ticket.updated_at = cx.now.clone();
    cx.tx.save_ticket(&ticket).await?;
    Ok(ticket)
}

/// A Thread open tickets follow closed: each one done with its Threads ends, unless its work has not finished.
pub(crate) async fn thread_closed(
    cx: &mut Cx<'_>,
    thread: &ThreadRef,
    outcome: CloseOutcome,
    at: &str,
) -> anyhow::Result<()> {
    let closed = thread_key(thread);
    for mut ticket in cx.tx.tickets_open().await? {
        let Some(follow) = ticket.follows.as_mut().and_then(|f| {
            f.iter_mut()
                .find(|item| item.thread == closed && item.closed.is_none())
        }) else {
            continue;
        };
        follow.closed = Some(Closed {
            at: at.to_owned(),
            outcome,
        });
        cx.tx.save_ticket(&ticket).await?;
        let Some(done) = follows_done(&ticket) else {
            continue;
        };
        if unfinished(cx, &ticket, at).await? {
            continue;
        }
        // Work resumed after the close and finished ends now.
        let resumed = latest_assignment(&ticket).is_some_and(|a| a.created_at.as_str() > at);
        let ended_at = if resumed {
            cx.now.clone()
        } else {
            done.at.clone()
        };
        end(
            cx,
            &ticket.id,
            TicketEndBy::Executor,
            close_outcome(done.outcome),
            Some(&ended_at),
            None,
        )
        .await?;
    }
    Ok(())
}

/// How a ticket that follows Threads is done with them: once no Thread it opened is open, or, when
/// it opened none, once every one it follows closed; the latest close among them. None while not done.
pub(crate) fn follows_done(ticket: &Ticket) -> Option<Closed> {
    let follows = ticket.follows.clone().unwrap_or_default();
    let own: Vec<&Follow> = follows.iter().filter(|f| f.own).collect();
    let counted: Vec<&Follow> = if own.is_empty() {
        follows.iter().collect()
    } else {
        own
    };
    if counted.is_empty() || counted.iter().any(|f| f.closed.is_none()) {
        return None;
    }
    counted
        .iter()
        .filter_map(|f| f.closed.clone())
        .max_by(|a, b| a.at.cmp(&b.at))
}

/// The Trigger to attach to a ticket, once the ticket may take it.
pub(crate) async fn attachable(
    cx: &mut Cx<'_>,
    ticket_id: &str,
    trigger_id: &str,
) -> anyhow::Result<Trigger> {
    let trigger = cx.require_trigger(trigger_id).await?;
    cx.left_to_workers(&trigger).await?;
    let current = cx.require(ticket_id).await?;
    if current.ended.as_ref().map(|e| e.by) == Some(TicketEndBy::Human) {
        return Err(refuse("ticketStopped", &[]));
    }
    if !live_ticket(&current) && !cx.policy.revalidate(&current.mandate) {
        return Err(refuse("mandateRevoked", &[]));
    }
    Ok(trigger)
}

/// Subscribes a ticket to an attached Trigger's Threads and adds the Trigger as an Input, with its note.
pub(crate) async fn attach(
    cx: &mut Cx<'_>,
    ticket_id: &str,
    trigger: &Trigger,
    note: Option<&str>,
) -> anyhow::Result<()> {
    for reference in &trigger.threads {
        if super::thread_rules::own_ticket_of(reference).is_some_and(|own| own != ticket_id) {
            continue;
        }
        subscribe(cx, ticket_id, reference).await?;
    }
    let current = cx
        .tx
        .disposition(&trigger.id)
        .await?
        .unwrap_or_else(|| TriggerDisposition {
            at: cx.now.clone(),
            ..TriggerDisposition::default()
        });
    let was_context = current
        .context
        .as_ref()
        .is_some_and(|c| c.contains(&ticket_id.to_owned()));
    let was_requeued = current.requeued == Some(true);
    let was_reached = current.fanout.iter().any(|id| id == ticket_id);
    let mut disposition = current.clone();
    disposition.requeued = None;
    disposition.context = current
        .context
        .as_ref()
        .map(|c| {
            c.iter()
                .filter(|id| *id != ticket_id)
                .cloned()
                .collect::<Vec<_>>()
        })
        .filter(|c| !c.is_empty());
    if !disposition.fanout.iter().any(|id| id == ticket_id) {
        disposition.fanout.push(ticket_id.to_owned());
    }
    cx.tx.save_disposition(&trigger.id, &disposition).await?;
    let mut ticket = cx.require(ticket_id).await?;
    let id = key(["input", ticket_id, "trigger", trigger.id.as_str()]);
    let at = cx.now.clone();
    let input = |input_id: String| TicketInput::Trigger {
        id: input_id,
        trigger_id: trigger.id.clone(),
        at: at.clone(),
        note: note.map(str::to_owned),
        human: trigger.human.then_some(true),
        context: None,
        read_at: None,
        delivered_to: None,
        delivered_at: None,
    };
    let existing = ticket.inputs.iter().position(|i| i.id() == id);
    match existing {
        None => add_input(cx, &mut ticket, input(id)).await?,
        Some(index) => {
            let waiting = ticket.undelivered.contains(&id);
            if let (Some(text), true) = (note, waiting) {
                if let TicketInput::Trigger { note, .. } = &mut ticket.inputs[index] {
                    *note = Some(text.to_owned());
                }
                ticket.updated_at = at.clone();
                cx.tx.save_ticket(&ticket).await?;
            } else if was_requeued || !was_reached || was_context || !live_ticket(&ticket) {
                // The worker gets it again: a person sent it to Eval, or the ticket let it go, read it
                // for context only, or ended after its worker had it.
                let again = key([
                    "input",
                    ticket_id,
                    "trigger",
                    trigger.id.as_str(),
                    "attached",
                    at.as_str(),
                ]);
                add_input(cx, &mut ticket, input(again)).await?;
            } else if let Some(text) = note {
                // The worker has the message already, so the note comes with it again.
                let noted = key([
                    "input",
                    ticket_id,
                    "trigger",
                    trigger.id.as_str(),
                    "note",
                    text,
                ]);
                add_input(cx, &mut ticket, input(noted)).await?;
            }
        }
    }
    Ok(())
}

/// Asks a running worker whose latest report is `waiting` for a status report.
pub(crate) async fn request_status(cx: &mut Cx<'_>, ticket_id: &str) -> anyhow::Result<bool> {
    let mut ticket = cx.require(ticket_id).await?;
    let waiting = running_assignment(&ticket)
        .and_then(|a| a.report.as_ref())
        .is_some_and(|r| r.state == ReportState::Waiting);
    if !live_ticket(&ticket) || !waiting {
        return Ok(false);
    }
    let input = TicketInput::Text {
        id: format!("status:{ticket_id}:{}", ticket.updated_at),
        text: STATUS_REQUEST.to_owned(),
        at: cx.now.clone(),
    };
    add_input(cx, &mut ticket, input).await?;
    Ok(true)
}

/// Stops a ticket on behalf of a person, ending its active Assignments.
pub(crate) async fn stop(
    cx: &mut Cx<'_>,
    ticket_id: &str,
    reason: Option<&str>,
) -> anyhow::Result<Ticket> {
    end(
        cx,
        ticket_id,
        TicketEndBy::Human,
        TicketOutcome::Stopped,
        None,
        reason,
    )
    .await?;
    cx.require(ticket_id).await
}

/// Ends the stalled Assignment of a retryable open ticket and starts a new one.
pub(crate) async fn retry(cx: &mut Cx<'_>, ticket_id: &str) -> anyhow::Result<Ticket> {
    let mut ticket = cx.require(ticket_id).await?;
    let Some(latest) = latest_assignment(&ticket).cloned() else {
        return Err(refuse("notRetryable", &[]));
    };
    if !can_retry(&ticket) {
        return Err(refuse("notRetryable", &[]));
    }
    end_assignment(cx, &mut ticket, &latest.id, "retried");
    requeue_untaken(&mut ticket, &latest.id);
    let assignment = new_assignment(&cx.now, &mut ticket);
    ticket.updated_at = cx.now.clone();
    cx.tx.save_ticket(&ticket).await?;
    cx.executor_stop(&ticket, &latest, "retried");
    cx.executor_start(&ticket, &assignment).await?;
    Ok(ticket)
}

/// Drops a ticket's Subscription to a Thread its worker found unrelated, and hands back the person's
/// messages there that the ticket decides.
pub(crate) async fn mark_unrelated(
    cx: &mut Cx<'_>,
    ticket_id: &str,
    thread: &str,
    reason: &str,
) -> anyhow::Result<()> {
    let ticket = cx.require(ticket_id).await?;
    if cx
        .source(&ticket)
        .await?
        .is_some_and(|t| t.threads.iter().any(|r| thread_key(r) == thread))
    {
        return Err(refuse("sourceThread", &[]));
    }
    if ticket
        .follows
        .as_ref()
        .is_some_and(|f| f.iter().any(|item| item.thread == thread))
    {
        return Err(refuse("followedThread", &[]));
    }
    let subscribed = cx
        .tx
        .thread(thread)
        .await?
        .is_some_and(|t| t.subscriptions.iter().any(|id| id == ticket_id));
    if !subscribed {
        return Err(refuse("notFollowing", &[]));
    }
    drop_subscriptions(cx, thread, "executor", Some(ticket_id), Some(reason)).await?;
    for trigger in triggers_of(cx, &ticket, None).await? {
        if trigger.threads.iter().any(|r| thread_key(r) == thread)
            && cx.owners(&trigger).await?.iter().any(|id| id == ticket_id)
        {
            cx.hand_back(ticket_id, &trigger, reason).await?;
        }
    }
    Ok(())
}

/// Tells each open ticket a Policy change bears on how it does, oldest first.
pub(crate) async fn policy_changed(
    cx: &mut Cx<'_>,
    from: &str,
    to: &str,
    effects: &HashMap<String, PolicyEffect>,
) -> anyhow::Result<()> {
    let at = cx.now.clone();
    let mut tickets = cx.tx.tickets_open().await?;
    tickets.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    for mut ticket in tickets {
        if let Some(effect) = effects.get(&ticket.id) {
            let input = TicketInput::PolicyChanged {
                id: key(["input", ticket.id.as_str(), "policy", from, to, at.as_str()]),
                from: from.to_owned(),
                to: to.to_owned(),
                at: at.clone(),
                effect: Some(effect.clone()),
            };
            add_input(cx, &mut ticket, input).await?;
        }
    }
    Ok(())
}

/// Drops a Thread's Subscriptions, every ticket's or only one's; false when none dropped.
pub(crate) async fn drop_subscriptions(
    cx: &mut Cx<'_>,
    thread: &str,
    by: &str,
    ticket_id: Option<&str>,
    reason: Option<&str>,
) -> anyhow::Result<bool> {
    let Some(mut current) = cx.tx.thread(thread).await? else {
        return Ok(false);
    };
    let dropped: Vec<String> = current
        .subscriptions
        .iter()
        .filter(|id| ticket_id.is_none_or(|t| t == id.as_str()))
        .cloned()
        .collect();
    if dropped.is_empty() {
        return Ok(false);
    }
    current.subscriptions.retain(|id| !dropped.contains(id));
    cx.tx.save_thread(&current).await?;
    for id in &dropped {
        let Some(mut ticket) = cx.tx.ticket(id).await? else {
            continue;
        };
        let Some(follows) = ticket.follows.as_mut() else {
            continue;
        };
        if !follows.iter().any(|item| item.thread == thread) {
            continue;
        }
        follows.retain(|item| item.thread != thread);
        if follows.is_empty() {
            ticket.follows = None;
        }
        cx.tx.save_ticket(&ticket).await?;
    }
    for id in &dropped {
        cx.emit(DomainEvent::ThreadUnsubscribed {
            thread_key: current.key.clone(),
            ticket_id: id.clone(),
            by: by.to_owned(),
            reason: reason.map(str::to_owned),
        });
    }
    Ok(true)
}

/// Adds an Input once and asks the executor to deliver it, or to launch again a pending Assignment
/// whose launch failed, once no person is waited for.
pub(crate) async fn add_input(
    cx: &mut Cx<'_>,
    ticket: &mut Ticket,
    input: TicketInput,
) -> anyhow::Result<()> {
    if ticket
        .inputs
        .iter()
        .any(|existing| existing.id() == input.id())
    {
        return Ok(());
    }
    let id = input.id().to_owned();
    // Automated Inputs wait while the worker waits for a person.
    let held = live_ticket(ticket) && automatic(Some(&input)) && waits_for_person(ticket);
    ticket.inputs.push(input);
    ticket.updated_at = cx.now.clone();
    if !held {
        ticket.undelivered.push(id);
    }
    cx.tx.save_ticket(ticket).await?;
    if !live_ticket(ticket) || held {
        return Ok(());
    }
    if running_assignment(ticket).is_some() {
        cx.executor_deliver(ticket).await?;
    } else if let Some(latest) = latest_assignment(ticket).cloned()
        && latest.state == AssignmentState::Pending
        && !waits_for_person(ticket)
    {
        cx.executor_start(ticket, &latest).await?;
    }
    Ok(())
}

/// Resumes an ended ticket under its unchanged Mandate when a person's message waits for it.
pub(crate) async fn wake(cx: &mut Cx<'_>, ticket_id: &str) -> anyhow::Result<()> {
    let Some(mut ticket) = cx.tx.ticket(ticket_id).await? else {
        return Ok(());
    };
    if live_ticket(&ticket)
        || ticket.ended.as_ref().map(|e| e.by) == Some(TicketEndBy::Human)
        || ticket.undelivered.is_empty()
    {
        return Ok(());
    }
    if cx.policy.paused() {
        return Ok(());
    }
    if !cx.policy.revalidate(&ticket.mandate) {
        refuse_waiting(cx, &mut ticket).await?;
        return Ok(());
    }
    // Only a person's message resumes an ended ticket.
    let first = ticket
        .undelivered
        .iter()
        .filter_map(|id| ticket.inputs.iter().find(|input| input.id() == id))
        .find(|input| {
            matches!(
                input,
                TicketInput::Trigger {
                    human: Some(true),
                    ..
                }
            )
        })
        .and_then(|input| input.trigger_id().map(str::to_owned));
    let Some(trigger_id) = first else {
        return Ok(());
    };
    let at = cx.now.clone();
    ticket.ended = None;
    ticket.updated_at = at.clone();
    let assignment = new_assignment(&at, &mut ticket);
    let resumed_id = key(["input", ticket_id, "resumed", trigger_id.as_str()]);
    ticket.inputs.push(TicketInput::Resumed {
        id: resumed_id.clone(),
        trigger_id: trigger_id.clone(),
        at,
    });
    ticket.undelivered.insert(0, resumed_id);
    cx.tx.save_ticket(&ticket).await?;
    cx.executor_start(&ticket, &assignment).await?;
    cx.emit(DomainEvent::TicketResumed {
        ticket_id: ticket_id.to_owned(),
        trigger_id,
    });
    Ok(())
}

/// The Triggers of a ticket's trigger Inputs, or of those whose Input ids are `ids`.
pub(crate) async fn triggers_of(
    cx: &mut Cx<'_>,
    ticket: &Ticket,
    ids: Option<&[String]>,
) -> anyhow::Result<Vec<Trigger>> {
    let mut triggers = Vec::new();
    for input in &ticket.inputs {
        if let TicketInput::Trigger { id, trigger_id, .. } = input
            && ids.is_none_or(|ids| ids.contains(id))
            && let Some(trigger) = cx.tx.trigger(trigger_id).await?
        {
            triggers.push(trigger);
        }
    }
    Ok(triggers)
}

/// Whether a ticket's worker took a person's message up, by acknowledging or answering it.
pub(crate) async fn taken_up(
    cx: &mut Cx<'_>,
    ticket: &Ticket,
    trigger: &Trigger,
) -> anyhow::Result<bool> {
    match cx.tx.disposition(&trigger.id).await?.and_then(|d| d.acked) {
        Some(acked) => Ok(acked.contains(&ticket.id)),
        None => Ok(cx
            .outbox_status(&key(["ack", trigger.id.as_str()]))
            .await?
            .is_some()),
    }
}

/// The person's messages among a ticket's Inputs `ids` that it decides and its worker did not take up.
async fn untaken(cx: &mut Cx<'_>, ticket: &Ticket, ids: &[String]) -> anyhow::Result<Vec<Trigger>> {
    let mut left = Vec::new();
    for trigger in triggers_of(cx, ticket, Some(ids)).await? {
        if cx.owners(&trigger).await?.contains(&ticket.id)
            && !taken_up(cx, ticket, &trigger).await?
        {
            left.push(trigger);
        }
    }
    Ok(left)
}

/// A ticket that ended, or cannot resume, lets go of the messages its worker never received and of
/// the person's messages it decided but may not have taken up; each of the latter that no other
/// ticket decides goes to Eval, unless a Decision settled it or a stop ended this ticket on it.
async fn let_go(
    cx: &mut Cx<'_>,
    ticket_id: &str,
    waiting: &[Trigger],
    left: &[Trigger],
) -> anyhow::Result<()> {
    for trigger in waiting.iter().chain(left.iter()) {
        if let Some(mut disposition) = cx.tx.disposition(&trigger.id).await?
            && disposition.fanout.iter().any(|id| id == ticket_id)
        {
            disposition.fanout.retain(|id| id != ticket_id);
            cx.tx.save_disposition(&trigger.id, &disposition).await?;
        }
    }
    for trigger in left {
        let disposition = cx.tx.disposition(&trigger.id).await?;
        let decision = match disposition.as_ref().and_then(|d| d.decision_id.clone()) {
            Some(id) => cx.tx.decision(&id).await?,
            None => None,
        };
        let handed_back = disposition
            .as_ref()
            .is_some_and(|d| d.handbacks.as_ref().is_some_and(|h| !h.is_empty()));
        let stopped = decision.as_ref().is_some_and(|d| {
            d.kind == DecisionKind::Stop && d.ticket_id.as_deref() == Some(ticket_id)
        }) && !handed_back;
        let decided = decision.as_ref().is_some_and(|d| settled(d) || stopped);
        let acked = disposition
            .as_ref()
            .is_some_and(|d| d.acked.as_ref().is_some_and(|a| !a.is_empty()));
        if cx.owners(trigger).await?.is_empty() && !acked && !decided {
            cx.queue_for_eval(trigger).await?;
        }
    }
    Ok(())
}

/// An ended ticket that cannot resume takes none of the messages that waited for it.
async fn refuse_waiting(cx: &mut Cx<'_>, ticket: &mut Ticket) -> anyhow::Result<()> {
    let waiting = triggers_of(cx, ticket, Some(&ticket.undelivered)).await?;
    let left = untaken(cx, ticket, &ticket.undelivered.clone()).await?;
    ticket.undelivered.clear();
    cx.tx.save_ticket(ticket).await?;
    let_go(cx, &ticket.id, &waiting, &left).await
}

/// Replaces a ticket's running Assignment with a new one that receives `note` first.
pub(crate) async fn restart(
    cx: &mut Cx<'_>,
    ticket: &mut Ticket,
    running_id: &str,
    reason: &str,
    note: &str,
) -> anyhow::Result<()> {
    end_assignment(cx, ticket, running_id, reason);
    requeue_untaken(ticket, running_id);
    let assignment = new_assignment(&cx.now, ticket);
    let at = cx.now.clone();
    let id = key([
        "input",
        ticket.id.as_str(),
        "restart",
        assignment.id.as_str(),
    ]);
    ticket.inputs.push(TicketInput::Text {
        id: id.clone(),
        text: note.to_owned(),
        at: at.clone(),
    });
    ticket.undelivered.push(id);
    ticket.updated_at = at;
    cx.tx.save_ticket(ticket).await?;
    cx.executor_start(ticket, &assignment).await
}

/// Puts back, ahead of what waits, the Trigger Inputs delivered to `replaced` that no step of its worker took.
fn requeue_untaken(ticket: &mut Ticket, replaced: &str) {
    let untaken: Vec<String> = ticket
        .inputs
        .iter()
        .filter_map(|input| match input {
            TicketInput::Trigger {
                id,
                read_at: Some(None),
                delivered_to: Some(to),
                ..
            } if to == replaced && !ticket.undelivered.contains(id) => Some(id.clone()),
            _ => None,
        })
        .collect();
    let mut undelivered = untaken;
    undelivered.append(&mut ticket.undelivered);
    ticket.undelivered = undelivered;
}

/// Ends a ticket with its outcome: `at` records when the ending fact happened, and `reason` why in
/// words. The ticket lets go of the messages its worker never received, and of the person's
/// messages it decides that its worker did not read before its latest report or turn end and
/// neither acknowledged, answered nor handed back.
pub(crate) async fn end(
    cx: &mut Cx<'_>,
    ticket_id: &str,
    by: TicketEndBy,
    outcome: TicketOutcome,
    at: Option<&str>,
    reason: Option<&str>,
) -> anyhow::Result<()> {
    let mut ticket = cx.require(ticket_id).await?;
    if !live_ticket(&ticket) {
        return Ok(());
    }
    let at = at.map(str::to_owned).unwrap_or_else(|| cx.now.clone());
    let stopping: Vec<Assignment> = ticket
        .assignments
        .iter()
        .filter(|a| a.state != AssignmentState::Ended)
        .cloned()
        .collect();
    let why = reason
        .filter(|r| *r != outcome_name(outcome))
        .map(str::to_owned);
    let until = read_until(&ticket, true);
    let waiting = triggers_of(cx, &ticket, Some(&ticket.undelivered)).await?;
    let left_ids: Vec<String> = ticket
        .inputs
        .iter()
        .filter(|input| {
            matches!(input, TicketInput::Trigger { .. })
                && (!read_before(input, &until)
                    || ticket.undelivered.contains(&input.id().to_owned()))
        })
        .map(|input| input.id().to_owned())
        .collect();
    let left = untaken(cx, &ticket, &left_ids).await?;
    ticket.ended = Some(Ended {
        at: at.clone(),
        by,
        outcome,
        reason: why.clone(),
    });
    ticket.undelivered.clear();
    ticket.updated_at = cx.now.clone();
    ticket.attention = None;
    ticket.delivery = None;
    for assignment in ticket.assignments.iter_mut() {
        if stopping.iter().any(|s| s.id == assignment.id) {
            assignment.state = AssignmentState::Ended;
            assignment.ended_at = Some(at.clone());
        }
    }
    cx.tx.save_ticket(&ticket).await?;
    let_go(cx, ticket_id, &waiting, &left).await?;
    cx.emit(DomainEvent::TicketEnded {
        ticket_id: ticket_id.to_owned(),
        by,
        outcome,
        reason: why,
    });
    let said = reason.unwrap_or(outcome_name(outcome));
    for assignment in &stopping {
        cx.executor_stop(&ticket, assignment, said);
    }
    Ok(())
}

/// Whether a ticket's work has not finished at a close at `at`.
async fn unfinished(cx: &mut Cx<'_>, ticket: &Ticket, at: &str) -> anyhow::Result<bool> {
    let until = read_until(ticket, false);
    let unread_ids: Vec<String> = ticket
        .inputs
        .iter()
        .filter(|input| matches!(input, TicketInput::Trigger { .. }) && !read_before(input, &until))
        .map(|input| input.id().to_owned())
        .collect();
    let mut taken = false;
    for trigger in triggers_of(cx, ticket, Some(&unread_ids)).await? {
        if cx.owners(&trigger).await?.contains(&ticket.id) && taken_up(cx, ticket, &trigger).await?
        {
            taken = true;
            break;
        }
    }
    let latest = latest_assignment(ticket);
    let resumed_after = latest.is_some_and(|a| a.created_at.as_str() > at);
    if taken || !resumed_after {
        return Ok(taken);
    }
    let report = latest.and_then(|a| a.report.as_ref());
    let report_at = report.map(|r| r.at.as_str()).unwrap_or("");
    let later = ticket
        .inputs
        .iter()
        .any(|input| !matches!(input, TicketInput::PolicyChanged { .. }) && input.at() > report_at);
    Ok(report.is_none_or(|r| r.state != ReportState::Ready)
        || !ticket.undelivered.is_empty()
        || later)
}

/// Adds a pending Assignment; a failed delivery to the one before no longer concerns the ticket.
pub(crate) fn new_assignment(now: &str, ticket: &mut Ticket) -> Assignment {
    let assignment = Assignment::new(
        &key([
            "assignment",
            ticket.id.as_str(),
            &ticket.assignments.len().to_string(),
        ]),
        now,
    );
    ticket.assignments.push(assignment.clone());
    ticket.delivery = None;
    assignment
}

pub(crate) fn end_assignment(
    cx: &mut Cx<'_>,
    ticket: &mut Ticket,
    assignment_id: &str,
    reason: &str,
) {
    if let Some(assignment) = ticket
        .assignments
        .iter_mut()
        .find(|a| a.id == assignment_id)
    {
        assignment.state = AssignmentState::Ended;
        assignment.ended_at = Some(cx.now.clone());
        assignment.ended_reason = Some(reason.to_owned());
    }
    cx.emit(DomainEvent::AssignmentEnded {
        ticket_id: ticket.id.clone(),
        assignment_id: assignment_id.to_owned(),
        reason: reason.to_owned(),
    });
}

/// Gives an open ticket's worker a fact as a note Input, which grants nothing.
pub(crate) async fn note(cx: &mut Cx<'_>, ticket_id: &str, text: &str) -> anyhow::Result<Ticket> {
    let mut ticket = cx.require(ticket_id).await?;
    if !live_ticket(&ticket) {
        return Err(refuse("ticketEnded", &[]));
    }
    let at = cx.now.clone();
    let input = TicketInput::Note {
        id: key(["input", ticket_id, "note", at.as_str(), text]),
        text: text.to_owned(),
        at,
    };
    add_input(cx, &mut ticket, input).await?;
    cx.require(ticket_id).await
}

/// Subscribes an open ticket to a Thread its worker found related, for context; false when it follows it already.
pub(crate) async fn follow_for_context(
    cx: &mut Cx<'_>,
    ticket_id: &str,
    reference: &ThreadRef,
) -> anyhow::Result<bool> {
    if !live_ticket(&cx.require(ticket_id).await?) {
        return Err(refuse("ticketEnded", &[]));
    }
    if cx
        .thread(reference)
        .await?
        .subscriptions
        .iter()
        .any(|id| id == ticket_id)
    {
        return Ok(false);
    }
    subscribe(cx, ticket_id, reference).await?;
    let mut thread = cx.thread(reference).await?;
    thread
        .context
        .get_or_insert_with(Vec::new)
        .push(ticket_id.to_owned());
    cx.tx.save_thread(&thread).await?;
    Ok(true)
}

/// Subscribes a ticket to a Thread; later Triggers on it reach the ticket, as its own once it was for context.
pub(crate) async fn subscribe(
    cx: &mut Cx<'_>,
    ticket_id: &str,
    reference: &ThreadRef,
) -> anyhow::Result<()> {
    let mut thread = cx.thread(reference).await?;
    if !thread.subscriptions.iter().any(|id| id == ticket_id) {
        thread.subscriptions.push(ticket_id.to_owned());
        cx.emit(DomainEvent::ThreadSubscribed {
            thread_key: thread.key.clone(),
            ticket_id: ticket_id.to_owned(),
        });
    }
    if let Some(context) = thread.context.as_mut()
        && context.iter().any(|id| id == ticket_id)
    {
        context.retain(|id| id != ticket_id);
    }
    cx.tx.save_thread(&thread).await
}
