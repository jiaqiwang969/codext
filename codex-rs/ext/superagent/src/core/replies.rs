//! The engine's messages to conversations, each by the rule that allows it (prototype `src/core/replies.ts`).

use super::engine::Cx;
use super::engine::refuse;
use super::model::ChannelRef;
use super::model::ReportState;
use super::model::Ticket;
use super::model::TicketOutcome;
use super::model::Trigger;
use super::model::latest_assignment;
use super::model::live_ticket;
use super::model::ticket_outcome;
use super::ports::Reply;
use super::ports::ReplyPurpose;
use super::ports::ReplyRequest;
use crate::ids::key;
use crate::store::MessageKind;
use crate::store::OutboundMessage;
use crate::store::ReplyStatus;

/// Queues a message for the conversation its purpose names; each message is queued at most once.
pub(crate) async fn reply(cx: &mut Cx<'_>, request: &ReplyRequest) -> anyhow::Result<Reply> {
    match request.purpose {
        ReplyPurpose::Acknowledgement => acknowledge(cx, request).await,
        _ => answer(cx, request).await,
    }
}

/// Queues the acknowledgement an accept carries where the message that started the ticket was written.
pub(crate) async fn acknowledge_accepted(
    cx: &mut Cx<'_>,
    ticket_id: &str,
    trigger: &Trigger,
    text: &str,
) -> anyhow::Result<()> {
    if let Some(channel) = &trigger.channel
        && trigger.human
    {
        cx.outbox_send(OutboundMessage {
            id: key(["ack", trigger.id.as_str()]),
            channel: channel.clone(),
            text: text.to_owned(),
            kind: MessageKind::Acknowledgement,
            ticket_id: Some(ticket_id.to_owned()),
        })
        .await?;
    }
    Ok(())
}

/// Withdraws a ticket's question for a person from the outbox, unless it went out already.
pub(crate) async fn withdraw_attention(cx: &mut Cx<'_>, attention_id: &str) -> anyhow::Result<()> {
    cx.outbox_withdraw(&key(["attention-reply", attention_id]))
        .await
}

async fn acknowledge(cx: &mut Cx<'_>, request: &ReplyRequest) -> anyhow::Result<Reply> {
    let ticket_id = request
        .ticket_id
        .as_deref()
        .ok_or_else(|| refuse("unknownTicket", &[]))?;
    let ticket = cx.require(ticket_id).await?;
    if !live_ticket(&ticket) {
        return Err(refuse("ackEnded", &[]));
    }
    let trigger_id = request
        .trigger_id
        .as_deref()
        .ok_or_else(|| refuse("unknownTrigger", &[]))?;
    let trigger = cx.require_trigger(trigger_id).await?;
    // Threads followed only for context are read-only.
    if !cx.reaches(&ticket, &trigger, false).await? {
        return Err(refuse("notReached", &[]));
    }
    let Some(channel) = trigger.channel.clone() else {
        return Err(refuse("ackNoChannel", &[]));
    };
    let id = key(["ack", trigger.id.as_str()]);
    let existing = cx.outbox_status(&id).await?;
    if existing.is_none() && !trigger.human {
        return Err(refuse("ackNotPerson", &[]));
    }
    if existing.is_none() {
        cx.outbox_send(OutboundMessage {
            id: id.clone(),
            channel,
            text: request.text.clone(),
            kind: MessageKind::Acknowledgement,
            ticket_id: Some(ticket.id.clone()),
        })
        .await?;
    }
    cx.take_up(&ticket.id, &trigger.id).await?;
    Ok(Reply {
        status: cx.outbox_status(&id).await?.unwrap_or(ReplyStatus::Pending),
        id,
        reused: existing.is_some(),
    })
}

async fn answer(cx: &mut Cx<'_>, request: &ReplyRequest) -> anyhow::Result<Reply> {
    let trigger = match &request.trigger_id {
        Some(id) => Some(cx.require_trigger(id).await?),
        None => None,
    };
    let mut ticket = match &request.ticket_id {
        Some(id) => Some(cx.require(id).await?),
        None => match &trigger {
            Some(trigger) => {
                let mut found = None;
                for candidate in cx.tx.tickets().await? {
                    if cx.reaches(&candidate, trigger, true).await? {
                        found = Some(candidate);
                        break;
                    }
                }
                found
            }
            None => None,
        },
    };
    if request.purpose == ReplyPurpose::Result {
        let ready = ticket.as_ref().is_some_and(|ticket| {
            let outcome = ticket_outcome(ticket);
            let reported = live_ticket(ticket)
                && latest_assignment(ticket)
                    .and_then(|a| a.report.as_ref())
                    .is_some_and(|r| r.state == ReportState::Ready);
            matches!(
                outcome,
                Some(TicketOutcome::Merged | TicketOutcome::Completed | TicketOutcome::Dropped)
            ) || reported
        });
        if !ready {
            return Err(refuse("notReady", &[]));
        }
    }
    let attention = if request.purpose == ReplyPurpose::Attention {
        ticket.as_ref().and_then(|t| t.attention.clone())
    } else {
        None
    };
    if request.purpose == ReplyPurpose::Attention
        && attention
            .as_ref()
            .is_none_or(|a| Some(&a.question) != request.question.as_ref())
    {
        return Err(refuse("notAttention", &[]));
    }
    if request.purpose == ReplyPurpose::Answer && !trigger.as_ref().is_some_and(|t| t.human) {
        return Err(refuse("notQuestion", &[]));
    }
    let id = match &attention {
        Some(attention) => key(["attention-reply", attention.id.as_str()]),
        None => key([
            "reply",
            trigger
                .as_ref()
                .map(|t| t.id.as_str())
                .or(ticket.as_ref().map(|t| t.id.as_str()))
                .unwrap_or(""),
            request.text.as_str(),
        ]),
    };
    let existing = cx.outbox_status(&id).await?;
    let answered = match (&ticket, &trigger) {
        (Some(ticket), Some(trigger)) => ticket
            .answers
            .as_ref()
            .is_some_and(|a| a.contains(&trigger.id)),
        _ => true,
    };
    if request.purpose == ReplyPurpose::Answer && !answered && existing.is_none() {
        return Err(refuse("noAnswer", &[]));
    }
    let asker = match attention.as_ref().and_then(|a| a.trigger_id.clone()) {
        Some(id) => cx.tx.trigger(&id).await?,
        None => None,
    };
    let channel: Option<ChannelRef> = if request.purpose == ReplyPurpose::Answer {
        trigger.as_ref().and_then(|t| t.channel.clone())
    } else {
        match asker.and_then(|a| a.channel) {
            Some(channel) => Some(channel),
            None => match &ticket {
                Some(ticket) => source_channel(cx, ticket).await?,
                None => None,
            },
        }
    };
    let Some(channel) = channel else {
        return Err(refuse("noChannel", &[]));
    };
    cx.outbox_send(OutboundMessage {
        id: id.clone(),
        channel,
        text: request.text.clone(),
        kind: MessageKind::Reply,
        ticket_id: ticket.as_ref().map(|t| t.id.clone()),
    })
    .await?;
    if request.purpose == ReplyPurpose::Answer
        && let (Some(ticket), Some(trigger)) = (ticket.as_mut(), &trigger)
        && let Some(answers) = ticket.answers.as_mut()
        && answers.contains(&trigger.id)
    {
        answers.retain(|id| id != &trigger.id);
        cx.tx.save_ticket(ticket).await?;
    }
    Ok(Reply {
        status: cx.outbox_status(&id).await?.unwrap_or(ReplyStatus::Pending),
        id,
        reused: existing.is_some(),
    })
}

/// The conversation a ticket started in: its origin Trigger's channel, else its first channel.
async fn source_channel(cx: &mut Cx<'_>, ticket: &Ticket) -> anyhow::Result<Option<ChannelRef>> {
    Ok(match cx.source(ticket).await?.and_then(|t| t.channel) {
        Some(channel) => Some(channel),
        None => ticket.channels.first().cloned(),
    })
}
