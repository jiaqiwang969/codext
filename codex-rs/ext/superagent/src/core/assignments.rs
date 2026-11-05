//! What the executor and its workers report about the Assignments that run tickets
//! (prototype `src/core/assignments.ts`). T-14 owns the executor side; the rules stay here.

use super::engine::Cx;
use super::engine::refuse;
use super::model::Attention;
use super::model::LastActivity;
use super::model::Report;
use super::model::ReportState;
use super::model::Ticket;
use super::model::TicketEndBy;
use super::model::TicketInput;
use super::model::TicketOutcome;
use super::model::latest_assignment;
use super::model::live_ticket;
use super::model::running_assignment;
use super::ports::DomainEvent;
use super::ports::WorkerReport;
use super::replies;
use super::tickets;
use crate::ids::key;

pub(crate) const RECOVERY_NOTE: &str = "The host restarted during this ticket. Inspect the current worktree, git history and the ticket's evidence before continuing. Do not repeat an already completed external operation. Finish the original assignment and report its status.";

/// Marks a non-ended Assignment running and queues the delivery of what waits for it.
pub(crate) async fn started(
    cx: &mut Cx<'_>,
    ticket_id: &str,
    assignment_id: &str,
) -> anyhow::Result<()> {
    let mut ticket = cx.require(ticket_id).await?;
    let Some(index) = ticket
        .assignments
        .iter()
        .position(|a| a.id == assignment_id)
    else {
        return Ok(());
    };
    if ticket.assignments[index].state == super::model::AssignmentState::Ended {
        return Ok(());
    }
    ticket.assignments[index].state = super::model::AssignmentState::Running;
    if ticket.assignments[index].started_at.is_none() {
        ticket.assignments[index].started_at = Some(cx.now.clone());
    }
    ticket.updated_at = cx.now.clone();
    cx.tx.save_ticket(&ticket).await?;
    if live_ticket(&ticket) && !ticket.undelivered.is_empty() {
        cx.executor_deliver(&ticket).await?;
    }
    cx.emit(DomainEvent::AssignmentStarted {
        ticket_id: ticket_id.to_owned(),
        assignment_id: assignment_id.to_owned(),
    });
    Ok(())
}

/// Marks Inputs delivered on the ticket's running Assignment and clears a failed delivery.
pub(crate) async fn delivered(
    cx: &mut Cx<'_>,
    ticket_id: &str,
    assignment_id: &str,
    input_ids: &[String],
) -> anyhow::Result<()> {
    let Some(mut ticket) = running(cx, ticket_id, assignment_id).await? else {
        return Ok(());
    };
    ticket.undelivered.retain(|id| !input_ids.contains(id));
    ticket.updated_at = cx.now.clone();
    if let Some(assignment) = ticket
        .assignments
        .iter_mut()
        .find(|a| a.id == assignment_id)
    {
        assignment.delivered_at = Some(cx.now.clone());
    }
    for input in ticket.inputs.iter_mut() {
        if let TicketInput::Trigger {
            id,
            read_at,
            delivered_to,
            ..
        } = input
            && input_ids.contains(id)
        {
            if read_at.is_none() {
                *read_at = Some(None);
            }
            *delivered_to = Some(assignment_id.to_owned());
        }
    }
    ticket.delivery = None;
    cx.tx.save_ticket(&ticket).await
}

/// Records when the worker's thread of an open ticket took Inputs into a step.
pub(crate) async fn read(
    cx: &mut Cx<'_>,
    ticket_id: &str,
    input_ids: &[String],
) -> anyhow::Result<()> {
    let mut ticket = cx.require(ticket_id).await?;
    if !live_ticket(&ticket) {
        return Ok(());
    }
    for input in ticket.inputs.iter_mut() {
        if let TicketInput::Trigger { id, read_at, .. } = input
            && input_ids.contains(id)
            && !matches!(read_at, Some(Some(_)))
        {
            *read_at = Some(Some(cx.now.clone()));
        }
    }
    cx.tx.save_ticket(&ticket).await
}

/// Records the start of a worker turn on its open ticket.
pub(crate) async fn turn_started(
    cx: &mut Cx<'_>,
    ticket_id: &str,
    assignment_id: &str,
) -> anyhow::Result<()> {
    let Some(mut ticket) = running(cx, ticket_id, assignment_id).await? else {
        return Ok(());
    };
    ticket.last_activity = Some(LastActivity::turn_started(&cx.now));
    ticket.updated_at = cx.now.clone();
    cx.tx.save_ticket(&ticket).await
}

pub(crate) async fn turn_ended(
    cx: &mut Cx<'_>,
    ticket_id: &str,
    assignment_id: &str,
) -> anyhow::Result<()> {
    let Some(mut ticket) = running(cx, ticket_id, assignment_id).await? else {
        return Ok(());
    };
    if let Some(assignment) = ticket
        .assignments
        .iter_mut()
        .find(|a| a.id == assignment_id)
    {
        assignment.idle_at = Some(cx.now.clone());
    }
    ticket.last_activity = Some(LastActivity::turn_ended(&cx.now));
    ticket.updated_at = cx.now.clone();
    cx.tx.save_ticket(&ticket).await
}

pub(crate) async fn delivery_failed(
    cx: &mut Cx<'_>,
    ticket_id: &str,
    assignment_id: &str,
    error: &str,
) -> anyhow::Result<()> {
    let Some(mut ticket) = running(cx, ticket_id, assignment_id).await? else {
        return Ok(());
    };
    ticket.delivery = Some(super::model::DeliveryFailure {
        error: error.chars().take(300).collect(),
        at: cx.now.clone(),
    });
    cx.tx.save_ticket(&ticket).await
}

/// A running Assignment the host's stop interrupted: starts it again with a recovery note unless
/// its latest report settled it and no Input reached it since, in which case it keeps its thread.
pub(crate) async fn interrupted(
    cx: &mut Cx<'_>,
    ticket_id: &str,
    assignment_id: &str,
) -> anyhow::Result<()> {
    let Some(mut ticket) = running(cx, ticket_id, assignment_id).await? else {
        return Ok(());
    };
    let Some(running) = running_assignment(&ticket).cloned() else {
        return Ok(());
    };
    let busy = match (&running.delivered_at, &running.report) {
        (Some(delivered), Some(report)) => delivered >= &report.at,
        _ => false,
    };
    let settled = running.report.as_ref().is_some_and(|report| {
        matches!(
            report.state,
            ReportState::Waiting | ReportState::Ready | ReportState::Blocked | ReportState::Failed
        )
    }) && !busy;
    if !settled {
        tickets::restart(cx, &mut ticket, &running.id, "hostRestarted", RECOVERY_NOTE).await?;
    } else if !ticket.undelivered.is_empty() {
        cx.executor_deliver(&ticket).await?;
    }
    Ok(())
}

/// Records a worker's report on an open ticket's current Assignment: its question for a person is
/// asked, kept or cleared; a blocked or failed report holds automated Inputs back; a dropped
/// report ends the ticket, and a ready one ends it once it is done with the Threads it follows.
pub(crate) async fn report(
    cx: &mut Cx<'_>,
    ticket_id: &str,
    report: &WorkerReport,
) -> anyhow::Result<Ticket> {
    let mut ticket = cx.require(ticket_id).await?;
    if !live_ticket(&ticket) {
        return Err(refuse("ticketEnded", &[]));
    }
    let question = report.question.as_ref().and_then(|q| q.as_deref());
    if question.is_some() && (report.state == ReportState::Ready || report.answers.is_some()) {
        return Err(refuse("questionWithResult", &[]));
    }
    if report.asked_by.is_some() && question.is_none() {
        return Err(refuse("askerWithoutQuestion", &[]));
    }
    if let Some(asked_by) = &report.asked_by
        && !person(cx, &ticket, asked_by, true).await?
    {
        return Err(refuse("notAsker", &[]));
    }
    if let Some(answers) = &report.answers
        && !person(cx, &ticket, answers, false).await?
    {
        return Err(refuse("notAnswerable", &[]));
    }
    let Some(assignment_id) = running_assignment(&ticket)
        .or(latest_assignment(&ticket))
        .map(|a| a.id.clone())
    else {
        return Err(refuse("unknownAssignment", &[]));
    };
    let at = cx.now.clone();
    let previous = ticket.attention.clone();
    let asked = match question {
        Some(q) if previous.as_ref().is_none_or(|p| p.question != q) => Some(Attention {
            id: key(["attention", ticket_id, q, at.as_str()]),
            question: q.to_owned(),
            trigger_id: report.asked_by.clone(),
        }),
        _ => previous.clone(),
    };
    let cleared = matches!(report.question, Some(None)) || report.state == ReportState::Ready;
    let attention = if cleared { None } else { asked };
    ticket.attention = attention.clone();
    if let Some(previous) = &previous
        && attention.as_ref().map(|a| &a.id) != Some(&previous.id)
    {
        replies::withdraw_attention(cx, &previous.id).await?;
    }
    {
        let Some(assignment) = ticket
            .assignments
            .iter_mut()
            .find(|a| a.id == assignment_id)
        else {
            return Err(refuse("unknownAssignment", &[]));
        };
        if assignment.state == super::model::AssignmentState::Ended {
            return Err(refuse("assignmentEnded", &[]));
        }
        assignment.report = Some(Report {
            state: report.state,
            summary: report.summary.clone(),
            at: at.clone(),
        });
    }
    ticket.last_activity = Some(LastActivity::report(&at, &report.summary));
    ticket.updated_at = at.clone();
    if let Some(answers) = &report.answers {
        let list = ticket.answers.get_or_insert_with(Vec::new);
        if !list.contains(answers) {
            list.push(answers.clone());
        }
        cx.take_up(ticket_id, answers).await?;
    }
    if matches!(report.state, ReportState::Blocked | ReportState::Failed) {
        let inputs = ticket.inputs.clone();
        ticket
            .undelivered
            .retain(|id| !tickets::automatic(inputs.iter().find(|input| input.id() == id)));
    }
    cx.tx.save_ticket(&ticket).await?;
    let follows_done = tickets::follows_done(&ticket);
    let ready = if ticket.follows.as_ref().is_none_or(Vec::is_empty) {
        Some(TicketOutcome::Completed)
    } else {
        follows_done
            .as_ref()
            .map(|c| tickets::close_outcome(c.outcome))
    };
    let outcome = match report.state {
        ReportState::Dropped => Some(
            follows_done
                .as_ref()
                .map(|c| tickets::close_outcome(c.outcome))
                .unwrap_or(TicketOutcome::Dropped),
        ),
        ReportState::Ready => ready,
        _ => None,
    };
    if let Some(outcome) = outcome {
        hand_back_untaken(cx, &ticket, &assignment_id).await?;
        tickets::end(
            cx,
            ticket_id,
            TicketEndBy::Executor,
            outcome,
            Some(&at),
            None,
        )
        .await?;
    }
    cx.require(ticket_id).await
}

/// A person's follow-up delivered to the ending assignment that the worker neither acknowledged,
/// answered nor handed back goes back to the PM as handed back, so no message of a person's ends
/// with the ticket unseen (11/03: a worker wrote "已退回" in its report and called no tool).
async fn hand_back_untaken(
    cx: &mut Cx<'_>,
    ticket: &Ticket,
    assignment_id: &str,
) -> anyhow::Result<()> {
    let mut trigger_ids = Vec::new();
    for input in &ticket.inputs {
        if let super::model::TicketInput::Trigger {
            trigger_id,
            delivered_to: Some(delivered_to),
            human: Some(true),
            context,
            ..
        } = input
            && delivered_to == assignment_id
            && *context != Some(true)
            && *trigger_id != ticket.origin.trigger_id
            && !ticket.origin.trigger_ids.contains(trigger_id)
        {
            trigger_ids.push(trigger_id.clone());
        }
    }
    for trigger_id in trigger_ids {
        let Some(trigger) = cx.tx.trigger(&trigger_id).await? else {
            continue;
        };
        if tickets::taken_up(cx, ticket, &trigger).await? {
            continue;
        }
        let handed = cx
            .tx
            .disposition(&trigger_id)
            .await?
            .and_then(|d| d.handbacks)
            .is_some_and(|h| h.iter().any(|item| item.ticket_id == ticket.id));
        if handed {
            continue;
        }
        cx.hand_back(
            &ticket.id,
            &trigger,
            "worker 结束工单时没有处理这条输入（没有确认、没有回答、也没有退回）",
        )
        .await?;
    }
    Ok(())
}

/// Whether `id` is a person's message that reached the ticket on a Thread of its own, with a channel when required.
async fn person(cx: &mut Cx<'_>, ticket: &Ticket, id: &str, channel: bool) -> anyhow::Result<bool> {
    let Some(trigger) = cx.tx.trigger(id).await? else {
        return Ok(false);
    };
    if !trigger.human || (channel && trigger.channel.is_none()) {
        return Ok(false);
    }
    cx.reaches(ticket, &trigger, false).await
}

/// The ticket, while it is open and `assignment_id` runs it.
async fn running(
    cx: &mut Cx<'_>,
    ticket_id: &str,
    assignment_id: &str,
) -> anyhow::Result<Option<Ticket>> {
    let ticket = cx.require(ticket_id).await?;
    let runs =
        live_ticket(&ticket) && running_assignment(&ticket).is_some_and(|a| a.id == assignment_id);
    Ok(runs.then_some(ticket))
}
