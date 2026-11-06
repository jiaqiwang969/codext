//! Evals: the Thread-level assessments of queued Triggers, and the Decisions they end with
//! (prototype `src/core/evals.ts`).

use std::collections::HashMap;

use chrono::DateTime;
use chrono::Duration;
use chrono::SecondsFormat;
use chrono::Utc;
use serde_json::Map;
use serde_json::Value;

use super::engine::Cx;
use super::engine::refuse;
use super::model::ActorRef;
use super::model::Decision;
use super::model::DecisionKind;
use super::model::Evaluation;
use super::model::Mandate;
use super::model::MandateBasis;
use super::model::Strength;
use super::model::Thread;
use super::model::TicketEndBy;
use super::model::TicketOutcome;
use super::model::Trigger;
use super::model::TriggerDisposition;
use super::model::live_ticket;
use super::model::thread_key;
use super::ports::AcceptOutcome;
use super::ports::Candidate;
use super::ports::DomainEvent;
use super::ports::GateResult;
use super::ports::Outcome;
use super::reasons::Reason;
use super::tickets;
use crate::ids::key;

/// How many times in a row an expired Eval queues its Triggers again before it is recorded unavailable.
const REQUEUES: u32 = 2;

/// What an Eval ends with, as its Decision records it.
struct Finished {
    kind: DecisionKind,
    reason: Vec<Reason>,
    audit: Option<Map<String, Value>>,
    ticket_id: Option<String>,
    mandate: Option<Mandate>,
}

impl Finished {
    fn new(kind: DecisionKind, reason: Vec<Reason>) -> Self {
        Self {
            kind,
            reason,
            audit: None,
            ticket_id: None,
            mandate: None,
        }
    }

    fn audit(mut self, audit: Option<&Map<String, Value>>) -> Self {
        self.audit = audit.cloned();
        self
    }
}

fn stronger(a: Strength, b: Strength) -> Strength {
    if a == Strength::Explicit || b == Strength::Explicit {
        Strength::Explicit
    } else {
        Strength::Inferred
    }
}

fn parse(iso: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// `iso` plus `ms` milliseconds, in the store's form.
pub fn add_ms(iso: &str, ms: i64) -> String {
    parse(iso)
        .map(|t| (t + Duration::milliseconds(ms)).to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_else(|| iso.to_owned())
}

fn ms_between(from: &str, to: &str) -> i64 {
    match (parse(from), parse(to)) {
        (Some(from), Some(to)) => (to - from).num_milliseconds(),
        _ => 0,
    }
}

/// Expires a Thread's Eval past its deadline, unless processing is paused: then the Eval notes
/// when the pause began, and once processing resumes its deadline moves on by the pause's length.
/// An Eval whose candidate has not reached the PM does not expire.
pub(crate) async fn due(cx: &mut Cx<'_>, thread_key: &str) -> anyhow::Result<()> {
    let Some(mut thread) = cx.tx.thread(thread_key).await? else {
        return Ok(());
    };
    let Some(evaluation) = thread.evaluating.clone() else {
        return Ok(());
    };
    let paused = cx.policy.paused();
    if paused && evaluation.paused_at.is_some() {
        return Ok(());
    }
    let now = cx.now.clone();
    if paused {
        if let Some(current) = thread.evaluating.as_mut() {
            current.paused_at = Some(now);
        }
    } else if let Some(paused_at) = &evaluation.paused_at {
        let shift = ms_between(paused_at, &now);
        if let Some(current) = thread.evaluating.as_mut() {
            current.deadline = add_ms(&current.deadline, shift);
            current.paused_at = None;
        }
    } else if evaluation.delivered.is_some() && evaluation.deadline <= now {
        return expire(cx, &mut thread).await;
    } else {
        return Ok(());
    }
    cx.tx.save_thread(&thread).await
}

/// An Eval's candidate reached the PM: its deadline counts from now.
pub(crate) async fn delivered(cx: &mut Cx<'_>, eval_id: &str) -> anyhow::Result<()> {
    let threads = cx.tx.threads_active().await?;
    let Some(mut thread) = threads
        .into_iter()
        .find(|t| t.evaluating.as_ref().is_some_and(|e| e.id == eval_id))
    else {
        return Ok(());
    };
    let Some(evaluation) = thread.evaluating.as_mut() else {
        return Ok(());
    };
    if evaluation.delivered.is_some() {
        return Ok(());
    }
    let now = cx.now.clone();
    evaluation.deadline = add_ms(&now, cx.policy.eval_timeout_ms());
    evaluation.delivered = Some(now);
    cx.tx.save_thread(&thread).await
}

/// An Eval past its deadline queues its Triggers again for a fresh Eval, up to `REQUEUES` times in a
/// row; after that it is recorded unavailable.
async fn expire(cx: &mut Cx<'_>, thread: &mut Thread) -> anyhow::Result<()> {
    let expired = thread.expired.unwrap_or(0) + 1;
    let Some(evaluation) = thread.evaluating.clone() else {
        return Ok(());
    };
    if expired > REQUEUES {
        finish(
            cx,
            thread,
            &evaluation,
            Finished::new(DecisionKind::Unavailable, vec![Reason::code("timeout")]),
        )
        .await?;
        return Ok(());
    }
    thread.evaluating = None;
    thread.expired = Some(expired);
    let mut pending = evaluation.trigger_ids.clone();
    pending.extend(
        thread
            .pending
            .iter()
            .filter(|id| !evaluation.trigger_ids.contains(id))
            .cloned(),
    );
    thread.pending = pending;
    cx.tx.save_thread(thread).await?;
    cx.assessor_release(&evaluation.id);
    Ok(())
}

/// Starts one Eval of the Triggers queued on a Thread, judged by the gate, and asks the PM for it.
pub(crate) async fn start(cx: &mut Cx<'_>, thread_key: &str) -> anyhow::Result<()> {
    let Some(mut thread) = cx.tx.thread(thread_key).await? else {
        return Ok(());
    };
    let at = cx.now.clone();
    // A person's follow-up a ticket still decides is that ticket's: one an older version queued leaves the queue without a Decision.
    let mut followups = Vec::new();
    for id in thread.pending.clone() {
        if let Some(trigger) = cx.tx.trigger(&id).await?
            && !cx.owners(&trigger).await?.is_empty()
        {
            followups.push(id);
        }
    }
    if !followups.is_empty() {
        thread.pending.retain(|id| !followups.contains(id));
        cx.tx.save_thread(&thread).await?;
        for trigger_id in followups {
            cx.emit(DomainEvent::TriggerFollowup { trigger_id });
        }
        if thread.pending.is_empty() {
            return Ok(());
        }
    }
    // Triggers that already started a ticket lend no authority to a new Eval and spawn no second ticket.
    let accepted = accepted(cx, &thread.key).await?;
    let started: Vec<String> = thread
        .pending
        .iter()
        .filter(|id| accepted.contains_key(*id))
        .cloned()
        .collect();
    if !started.is_empty() {
        let reasons = started
            .iter()
            .map(|id| {
                Reason::code("startedTicket")
                    .with(
                        "ticket",
                        accepted
                            .get(id)
                            .and_then(|d| d.ticket_id.as_deref())
                            .unwrap_or(""),
                    )
                    .for_trigger(id)
            })
            .collect();
        ignore(cx, &mut thread, started, reasons, &at).await?;
    }
    let mut triggers = Vec::new();
    for id in &thread.pending {
        if let Some(trigger) = cx.tx.trigger(id).await? {
            triggers.push(trigger);
        }
    }
    let tickets = cx.tickets_on_thread(&thread).await?;
    let mut reasons = Vec::new();
    let mut strongest: Option<(Trigger, Strength, MandateBasis)> = None;
    for trigger in &triggers {
        match cx.policy.gate(trigger, &thread, &tickets) {
            GateResult::Inadmissible { reason } => {
                reasons.push(Reason::code(&reason).for_trigger(&trigger.id))
            }
            GateResult::Admissible { strength, basis } => {
                let replace = match &strongest {
                    None => true,
                    Some((current, current_strength, _)) => {
                        let later = strength == *current_strength && trigger.at >= current.at;
                        let weaker = stronger(*current_strength, strength) != *current_strength;
                        weaker || later
                    }
                };
                if replace {
                    strongest = Some((trigger.clone(), strength, basis));
                }
            }
        }
    }
    let Some((primary, strength, basis)) = strongest else {
        if !thread.pending.is_empty() {
            let ids = thread.pending.clone();
            let reasons = if reasons.is_empty() {
                vec![Reason::code("noAdmissible")]
            } else {
                reasons
            };
            ignore(cx, &mut thread, ids, reasons, &at).await?;
        }
        cx.tx.save_thread(&thread).await?;
        return Ok(());
    };
    let version = cx.policy.version();
    let mut parts: Vec<&str> = vec!["eval", &thread.key, &version, &at];
    parts.extend(thread.pending.iter().map(String::as_str));
    let eval_id = key(parts);
    let deadline = add_ms(&at, cx.policy.eval_timeout_ms());
    thread.evaluating = Some(Evaluation {
        id: eval_id.clone(),
        thread_key: thread.key.clone(),
        trigger_ids: thread.pending.clone(),
        primary: Some(primary.id.clone()),
        strength,
        actor: primary.actor.clone(),
        basis,
        policy_version: version,
        started_at: at.clone(),
        delivered: None,
        deadline,
        paused_at: None,
    });
    let trigger_ids = thread.pending.clone();
    thread.pending = Vec::new();
    cx.tx.save_thread(&thread).await?;
    for trigger in &triggers {
        let mut disposition =
            cx.tx
                .disposition(&trigger.id)
                .await?
                .unwrap_or_else(|| TriggerDisposition {
                    at: at.clone(),
                    ..TriggerDisposition::default()
                });
        disposition.eval_id = Some(eval_id.clone());
        cx.tx.save_disposition(&trigger.id, &disposition).await?;
    }
    cx.emit(DomainEvent::EvalStarted {
        eval_id,
        thread_key: thread.key.clone(),
        trigger_ids,
        strength,
    });
    let candidate = candidate_of(cx, &thread).await?;
    cx.assessor_request(&candidate).await
}

/// The candidate of the Eval that holds a Trigger, or none while no Eval holds it.
pub(crate) async fn candidate(
    cx: &mut Cx<'_>,
    trigger_id: &str,
) -> anyhow::Result<Option<Candidate>> {
    match holding(cx, trigger_id).await? {
        Some(thread) => Ok(Some(candidate_of(cx, &thread).await?)),
        None => Ok(None),
    }
}

/// Decides a Trigger: the Eval that holds it, or a direct Eval of it alone when none does.
pub(crate) async fn decide(
    cx: &mut Cx<'_>,
    trigger_id: &str,
    outcome: Outcome,
    eval_id: Option<&str>,
) -> anyhow::Result<Decision> {
    let trigger = cx.require_trigger(trigger_id).await?;
    let held = holding(cx, trigger_id).await?;
    if let Some(eval_id) = eval_id
        && held
            .as_ref()
            .and_then(|t| t.evaluating.as_ref())
            .map(|e| e.id.as_str())
            != Some(eval_id)
    {
        return Err(refuse("evalFinished", &[("eval", eval_id)]));
    }
    let Some(primary_ref) = trigger.threads.first().cloned() else {
        return Err(refuse("unknownTrigger", &[]));
    };
    // A replayed Trigger that already started a ticket starts no second one.
    let earlier = if matches!(outcome, Outcome::Accept(_)) {
        accepted(cx, &thread_key(&primary_ref))
            .await?
            .remove(trigger_id)
    } else {
        None
    };
    if let Some(earlier) = earlier {
        if let Some(mut held) = held {
            let others: Vec<String> = held
                .evaluating
                .as_ref()
                .map(|e| {
                    e.trigger_ids
                        .iter()
                        .filter(|id| *id != trigger_id)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            if !others.is_empty() {
                reassess(
                    cx,
                    &mut held,
                    Reason::code("acceptedBefore").with("trigger", trigger_id),
                    &[trigger_id.to_owned()],
                )
                .await?;
            } else if let Some(evaluation) = held.evaluating.clone() {
                let ticket = earlier.ticket_id.clone().unwrap_or_default();
                finish(
                    cx,
                    &mut held,
                    &evaluation,
                    Finished::new(
                        DecisionKind::Ignore,
                        vec![
                            Reason::code("startedTicket")
                                .with("ticket", &ticket)
                                .for_trigger(trigger_id),
                        ],
                    ),
                )
                .await?;
            }
        }
        return Ok(earlier);
    }
    // A follow-up that reached a ticket is its worker's to judge: the PM may only stop the work on it.
    if !matches!(outcome, Outcome::Stop { .. }) {
        cx.left_to_workers(&trigger).await?;
    }
    // The Eval that holds the Trigger decides it; any other Eval of its Thread goes on beside a direct one.
    let (mut thread, held_evaluation) = match held {
        Some(thread) => {
            let evaluation = thread.evaluating.clone();
            (thread, evaluation)
        }
        None => (cx.thread(&primary_ref).await?, None),
    };
    let evaluation = match held_evaluation {
        Some(evaluation) => evaluation,
        None => {
            let stop_ticket = match &outcome {
                Outcome::Stop { ticket_id, .. } => Some(ticket_id.as_str()),
                _ => None,
            };
            direct_eval(cx, &mut thread, &trigger, outcome.kind_name(), stop_ticket).await?
        }
    };
    if let Outcome::Accept(accept) = &outcome
        && evaluation.strength != Strength::Explicit
        && !cx
            .policy
            .admits_inferred(accept.rules.as_deref().unwrap_or(&[]))
    {
        return Err(refuse("notExplicit", &[]));
    }
    decide_eval(cx, &mut thread, evaluation, outcome).await
}

/// Queues a stored Trigger for a fresh Eval on its primary Thread; false while an Eval holds it.
pub(crate) async fn reevaluate(cx: &mut Cx<'_>, trigger_id: &str) -> anyhow::Result<bool> {
    if !requeue(cx, trigger_id).await? {
        return Ok(false);
    }
    cx.emit(DomainEvent::TriggerRequeued {
        trigger_id: trigger_id.to_owned(),
    });
    Ok(true)
}

async fn requeue(cx: &mut Cx<'_>, trigger_id: &str) -> anyhow::Result<bool> {
    let trigger = cx.require_trigger(trigger_id).await?;
    if holding(cx, trigger_id).await?.is_some() {
        return Ok(false);
    }
    cx.queue_for_eval(&trigger).await?;
    let mut disposition =
        cx.tx
            .disposition(trigger_id)
            .await?
            .unwrap_or_else(|| TriggerDisposition {
                at: cx.now.clone(),
                ..TriggerDisposition::default()
            });
    disposition.requeued = Some(true);
    cx.tx.save_disposition(trigger_id, &disposition).await?;
    Ok(true)
}

/// A ticket's worker hands back a Trigger that reached it as beyond the ticket; queued for a fresh
/// Eval when the ticket decided it and no other ticket still does. A ticket hands a Trigger back once.
pub(crate) async fn handback(
    cx: &mut Cx<'_>,
    ticket_id: &str,
    trigger_id: &str,
    reason: &str,
) -> anyhow::Result<bool> {
    let ticket = cx.require(ticket_id).await?;
    if !ticket
        .inputs
        .iter()
        .any(|input| input.trigger_id() == Some(trigger_id))
    {
        return Err(refuse("notReached", &[]));
    }
    let already = cx
        .tx
        .disposition(trigger_id)
        .await?
        .and_then(|d| d.handbacks)
        .is_some_and(|h| h.iter().any(|item| item.ticket_id == ticket_id));
    if already {
        return Err(refuse("handedBack", &[]));
    }
    let trigger = cx.require_trigger(trigger_id).await?;
    cx.hand_back(ticket_id, &trigger, reason).await
}

/// Marks an `unavailable` Decision dismissed.
pub(crate) async fn dismiss(cx: &mut Cx<'_>, decision_id: &str) -> anyhow::Result<Decision> {
    let Some(mut decision) = cx.tx.decision(decision_id).await? else {
        return Err(refuse("unknownProblem", &[]));
    };
    if decision.kind != DecisionKind::Unavailable {
        return Err(refuse("unknownProblem", &[]));
    }
    decision.dismissed = Some(true);
    cx.tx.save_decision(&decision).await?;
    Ok(decision)
}

/// Takes a Trigger out of its Thread's queue because it was routed by hand.
pub(crate) async fn withdraw(cx: &mut Cx<'_>, trigger: &Trigger) -> anyhow::Result<()> {
    let Some(reference) = trigger.threads.first() else {
        return Ok(());
    };
    let Some(mut thread) = cx.tx.thread(&thread_key(reference)).await? else {
        return Ok(());
    };
    if !thread.pending.contains(&trigger.id) {
        return Ok(());
    }
    thread.pending.retain(|id| id != &trigger.id);
    cx.tx.save_thread(&thread).await
}

/// Ends the Eval that holds a Trigger routed to a ticket; one that holds other Triggers too queues
/// them again for a fresh Eval without it.
pub(crate) async fn routed(
    cx: &mut Cx<'_>,
    trigger_id: &str,
    ticket_id: &str,
) -> anyhow::Result<()> {
    let Some(mut held) = holding(cx, trigger_id).await? else {
        return Ok(());
    };
    let reason = Reason::code("routed").with("ticket", ticket_id);
    let Some(evaluation) = held.evaluating.clone() else {
        return Ok(());
    };
    if evaluation.trigger_ids.len() == 1 {
        finish(
            cx,
            &mut held,
            &evaluation,
            Finished::new(DecisionKind::Ignore, vec![reason]),
        )
        .await?;
    } else {
        reassess(
            cx,
            &mut held,
            reason.for_trigger(trigger_id),
            &[trigger_id.to_owned()],
        )
        .await?;
    }
    Ok(())
}

/// Starts the Evals in progress again under the new Policy version, and says that it changed.
pub(crate) async fn policy_changed(cx: &mut Cx<'_>, from: &str, to: &str) -> anyhow::Result<()> {
    for mut thread in cx.tx.threads_active().await? {
        if thread.evaluating.is_some() {
            reassess(cx, &mut thread, Reason::code("policyChanged"), &[]).await?;
        }
    }
    cx.emit(DomainEvent::PolicyChanged {
        from: from.to_owned(),
        to: to.to_owned(),
    });
    Ok(())
}

/// Ends a Thread's Eval as unavailable for `reason` and queues its Triggers, except `without`, ahead of the others.
async fn reassess(
    cx: &mut Cx<'_>,
    thread: &mut Thread,
    reason: Reason,
    without: &[String],
) -> anyhow::Result<Decision> {
    let Some(evaluation) = thread.evaluating.clone() else {
        return Err(refuse("unknownProblem", &[]));
    };
    let ids: Vec<String> = evaluation
        .trigger_ids
        .iter()
        .filter(|id| !without.contains(id))
        .cloned()
        .collect();
    let decision = finish(
        cx,
        thread,
        &evaluation,
        Finished::new(DecisionKind::Unavailable, vec![reason]),
    )
    .await?;
    let mut current = cx
        .tx
        .thread(&thread.key)
        .await?
        .unwrap_or_else(|| thread.clone());
    let mut pending = ids.clone();
    pending.extend(
        current
            .pending
            .iter()
            .filter(|id| !ids.contains(id))
            .cloned(),
    );
    current.pending = pending;
    cx.tx.save_thread(&current).await?;
    *thread = current;
    Ok(decision)
}

/// The Thread whose Eval holds a Trigger: an Eval runs on the primary Thread of each Trigger it holds.
async fn holding(cx: &mut Cx<'_>, trigger_id: &str) -> anyhow::Result<Option<Thread>> {
    let Some(trigger) = cx.tx.trigger(trigger_id).await? else {
        return Ok(None);
    };
    let Some(reference) = trigger.threads.first() else {
        return Ok(None);
    };
    let Some(thread) = cx.tx.thread(&thread_key(reference)).await? else {
        return Ok(None);
    };
    let holds = thread
        .evaluating
        .as_ref()
        .is_some_and(|e| e.trigger_ids.iter().any(|id| id == trigger_id));
    Ok(holds.then_some(thread))
}

async fn candidate_of(cx: &mut Cx<'_>, thread: &Thread) -> anyhow::Result<Candidate> {
    let Some(evaluation) = thread.evaluating.clone() else {
        return Err(refuse("unknownProblem", &[]));
    };
    let mut triggers = Vec::new();
    for id in &evaluation.trigger_ids {
        if let Some(trigger) = cx.tx.trigger(id).await? {
            triggers.push(trigger);
        }
    }
    Ok(Candidate {
        eval_id: evaluation.id,
        thread: thread.clone(),
        triggers,
        strength: evaluation.strength,
        policy_version: evaluation.policy_version,
        tickets: cx.tickets_on_thread(thread).await?,
    })
}

/// Records that the queued Triggers `ids` of a Thread start no Eval, for `reason`, and takes them out of its queue.
async fn ignore(
    cx: &mut Cx<'_>,
    thread: &mut Thread,
    ids: Vec<String>,
    reason: Vec<Reason>,
    at: &str,
) -> anyhow::Result<()> {
    let version = cx.policy.version();
    let mut parts: Vec<&str> = vec!["eval", &thread.key, "ignored", &version, at];
    parts.extend(ids.iter().map(String::as_str));
    let evaluation = Evaluation {
        id: key(parts),
        thread_key: thread.key.clone(),
        trigger_ids: ids.clone(),
        primary: None,
        strength: Strength::Inferred,
        actor: ActorRef {
            source: thread.reference.source.clone(),
            id: String::new(),
        },
        basis: MandateBasis::via("followup"),
        policy_version: version,
        started_at: at.to_owned(),
        delivered: None,
        deadline: at.to_owned(),
        paused_at: None,
    };
    record(
        cx,
        thread,
        &evaluation,
        Finished::new(DecisionKind::Ignore, reason),
    )
    .await?;
    thread.pending.retain(|id| !ids.contains(id));
    thread.expired = None;
    Ok(())
}

/// The accept Decisions of the Triggers whose primary Thread is `thread_key`, by Trigger.
async fn accepted(cx: &mut Cx<'_>, thread_key: &str) -> anyhow::Result<HashMap<String, Decision>> {
    let mut accepted = HashMap::new();
    for decision in cx.tx.decisions_by_thread(thread_key).await? {
        if decision.kind != DecisionKind::Accept || decision.ticket_id.is_none() {
            continue;
        }
        for id in &decision.trigger_ids {
            accepted
                .entry(id.clone())
                .or_insert_with(|| decision.clone());
        }
    }
    Ok(accepted)
}

/// A gate-only Eval of one Trigger, for an outcome decided without a queued Eval.
async fn direct_eval(
    cx: &mut Cx<'_>,
    thread: &mut Thread,
    trigger: &Trigger,
    kind: &str,
    ticket_id: Option<&str>,
) -> anyhow::Result<Evaluation> {
    let tickets = cx.tickets_on_thread(thread).await?;
    let (strength, basis) = match cx.policy.gate(trigger, thread, &tickets) {
        GateResult::Inadmissible { reason } => {
            return Err(refuse("notAdmissible", &[("reason", &reason)]));
        }
        GateResult::Admissible { strength, basis } => (strength, basis),
    };
    let at = cx.now.clone();
    if thread.pending.contains(&trigger.id) {
        thread.pending.retain(|id| id != &trigger.id);
        cx.tx.save_thread(thread).await?;
    }
    let mut parts: Vec<&str> = vec!["eval", &thread.key, "direct", &trigger.id, kind];
    if let Some(ticket_id) = ticket_id {
        parts.push(ticket_id);
    }
    parts.push(&at);
    Ok(Evaluation {
        id: key(parts),
        thread_key: thread.key.clone(),
        trigger_ids: vec![trigger.id.clone()],
        primary: Some(trigger.id.clone()),
        strength,
        actor: trigger.actor.clone(),
        basis,
        policy_version: cx.policy.version(),
        started_at: at.clone(),
        delivered: None,
        deadline: at,
        paused_at: None,
    })
}

/// Applies an outcome to an Eval of a Thread: the one in progress, or a gate-only one.
async fn decide_eval(
    cx: &mut Cx<'_>,
    thread: &mut Thread,
    evaluation: Evaluation,
    outcome: Outcome,
) -> anyhow::Result<Decision> {
    let audit = outcome.audit().cloned();
    // The Policy changed without a live policy_changed call, such as across a restart.
    if cx.policy.version() != evaluation.policy_version {
        if thread
            .evaluating
            .as_ref()
            .is_some_and(|e| e.id == evaluation.id)
        {
            return reassess(cx, thread, Reason::code("policyChanged"), &[]).await;
        }
        return finish(
            cx,
            thread,
            &evaluation,
            Finished::new(
                DecisionKind::Unavailable,
                vec![Reason::code("policyChanged")],
            )
            .audit(audit.as_ref()),
        )
        .await;
    }
    match outcome {
        Outcome::Ignore { reason, .. } => {
            // An inferred candidate ignored without an assessment, from its card, says so beside the reason given.
            let mut reasons = Vec::new();
            if evaluation.strength != Strength::Explicit && audit.is_none() {
                reasons.push(Reason::code("ignoredFromCard"));
            }
            reasons.extend(reason.reasons());
            finish(
                cx,
                thread,
                &evaluation,
                Finished::new(DecisionKind::Ignore, reasons).audit(audit.as_ref()),
            )
            .await
        }
        Outcome::Unavailable { reason, .. } => {
            finish(
                cx,
                thread,
                &evaluation,
                Finished::new(DecisionKind::Unavailable, reason.reasons()).audit(audit.as_ref()),
            )
            .await
        }
        Outcome::Stop {
            ticket_id, reason, ..
        } => {
            let ticket = cx.tx.ticket(&ticket_id).await?.filter(live_ticket);
            let Some(ticket) = ticket else {
                return finish(
                    cx,
                    thread,
                    &evaluation,
                    Finished::new(DecisionKind::Ignore, vec![Reason::code("stopUnknown")])
                        .audit(audit.as_ref()),
                )
                .await;
            };
            if evaluation.strength != Strength::Explicit {
                return finish(
                    cx,
                    thread,
                    &evaluation,
                    Finished::new(DecisionKind::Ignore, vec![Reason::code("stopNotExplicit")])
                        .audit(audit.as_ref()),
                )
                .await;
            }
            let mut finished =
                Finished::new(DecisionKind::Stop, reason.reasons()).audit(audit.as_ref());
            finished.ticket_id = Some(ticket.id.clone());
            let decision = finish(cx, thread, &evaluation, finished).await?;
            tickets::end(
                cx,
                &ticket.id,
                TicketEndBy::Human,
                TicketOutcome::Stopped,
                None,
                reason.words(),
            )
            .await?;
            Ok(decision)
        }
        Outcome::Accept(accept_outcome) => accept(cx, thread, &evaluation, &accept_outcome).await,
    }
}

/// Records an accept with the Mandate the Eval gives, and creates its ticket.
async fn accept(
    cx: &mut Cx<'_>,
    thread: &mut Thread,
    evaluation: &Evaluation,
    outcome: &AcceptOutcome,
) -> anyhow::Result<Decision> {
    let at = cx.now.clone();
    let mut basis = evaluation.basis.clone();
    if let Some(rules) = &outcome.rules {
        basis.rules = Some(rules.clone());
    }
    let mandate = Mandate {
        strength: evaluation.strength,
        policy_version: evaluation.policy_version.clone(),
        basis,
        actor: evaluation.actor.clone(),
        scope: outcome.scope.clone(),
    };
    let ticket_id = key(["ticket", evaluation.id.as_str()]);
    let mut finished =
        Finished::new(DecisionKind::Accept, outcome.reason.reasons()).audit(outcome.audit.as_ref());
    finished.mandate = Some(mandate.clone());
    finished.ticket_id = Some(ticket_id.clone());
    let decision = finish(cx, thread, evaluation, finished).await?;
    tickets::create(
        cx,
        &ticket_id,
        &decision.id,
        mandate,
        evaluation,
        outcome,
        &at,
    )
    .await?;
    Ok(decision)
}

/// Records an Eval's Decision; the Thread's own Eval ends with it, while a gate-only one leaves it going.
async fn finish(
    cx: &mut Cx<'_>,
    thread: &mut Thread,
    evaluation: &Evaluation,
    outcome: Finished,
) -> anyhow::Result<Decision> {
    let decision = record(cx, thread, evaluation, outcome).await?;
    if thread.evaluating.as_ref().map(|e| e.id.as_str()) != Some(evaluation.id.as_str()) {
        return Ok(decision);
    }
    thread.evaluating = None;
    thread.expired = None;
    cx.tx.save_thread(thread).await?;
    cx.assessor_release(&evaluation.id);
    Ok(decision)
}

fn kind_name(kind: DecisionKind) -> &'static str {
    match kind {
        DecisionKind::Ignore => "ignore",
        DecisionKind::Accept => "accept",
        DecisionKind::Reopen => "reopen",
        DecisionKind::Stop => "stop",
        DecisionKind::Unavailable => "unavailable",
    }
}

async fn record(
    cx: &mut Cx<'_>,
    thread: &Thread,
    evaluation: &Evaluation,
    outcome: Finished,
) -> anyhow::Result<Decision> {
    let at = cx.now.clone();
    let decision = Decision {
        id: key(["decision", evaluation.id.as_str()]),
        eval_id: evaluation.id.clone(),
        thread_key: thread.key.clone(),
        trigger_ids: evaluation.trigger_ids.clone(),
        policy_version: evaluation.policy_version.clone(),
        at: at.clone(),
        kind: outcome.kind,
        reason: outcome.reason,
        strength: evaluation.strength,
        ticket_id: outcome.ticket_id,
        mandate: outcome.mandate,
        audit: outcome.audit,
        dismissed: None,
    };
    cx.tx.save_decision(&decision).await?;
    for id in &evaluation.trigger_ids {
        let mut disposition = cx
            .tx
            .disposition(id)
            .await?
            .unwrap_or_else(|| TriggerDisposition {
                at: at.clone(),
                ..TriggerDisposition::default()
            });
        disposition.eval_id = Some(evaluation.id.clone());
        disposition.decision_id = Some(decision.id.clone());
        cx.tx.save_disposition(id, &disposition).await?;
    }
    cx.emit(DomainEvent::DecisionRecorded {
        decision_id: decision.id.clone(),
        kind: kind_name(decision.kind).to_owned(),
        thread_key: thread.key.clone(),
    });
    Ok(decision)
}
