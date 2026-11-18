//! Trigger ingestion, thread correlation, subscription fan-out and Eval queuing (prototype `src/core/intake.ts`).

use super::engine::Cx;
use super::model::Ticket;
use super::model::TicketEndBy;
use super::model::TicketInput;
use super::model::Trigger;
use super::model::TriggerDisposition;
use super::model::iso_time;
use super::model::live_ticket;
use super::model::thread_key;
use super::ports::DomainEvent;
use super::ports::ReceiveResult;
use super::ports::ScopeResult;
use super::reasons::Reason;
use super::tickets;
use crate::ids::key;

/// Stores a Trigger once, fans it out to the tickets subscribed to its Threads (to an ended one only
/// a person's message, which resumes it, and to none a person stopped), and queues it for Eval on
/// its primary Thread unless a ticket's worker decides it.
pub(crate) async fn receive(cx: &mut Cx<'_>, received: Trigger) -> anyhow::Result<ReceiveResult> {
    let mut trigger = received;
    trigger.at = iso_time(&trigger.at);
    if trigger.threads.is_empty() {
        anyhow::bail!("A trigger must belong to at least one thread");
    }
    if cx.tx.trigger(&trigger.id).await?.is_some() {
        return Ok(ReceiveResult::Duplicate);
    }
    let at = cx.now.clone();
    if let ScopeResult::Filtered { reason } = cx.e.scope.scope(&trigger) {
        cx.tx.save_trigger(&trigger).await?;
        cx.tx
            .save_disposition(
                &trigger.id,
                &TriggerDisposition {
                    filtered: Some(Reason::code(&reason)),
                    at,
                    ..TriggerDisposition::default()
                },
            )
            .await?;
        return Ok(ReceiveResult::Filtered);
    }
    cx.tx.save_trigger(&trigger).await?;
    let keys: Vec<String> = trigger.threads.iter().map(thread_key).collect();
    // The tickets reached, in order, and whether each was reached only through a Thread it follows for context.
    let mut reached: Vec<(String, Ticket, bool)> = Vec::new();
    for reference in &trigger.threads {
        let mut thread = cx.thread(reference).await?;
        if thread
            .last_at
            .as_deref()
            .is_none_or(|last| trigger.at.as_str() > last)
        {
            thread.last_at = Some(trigger.at.clone());
        }
        for other in &keys {
            if other != &thread.key && !thread.correlated.contains(other) {
                thread.correlated.push(other.clone());
            }
        }
        // A ticket's own Thread reaches that ticket alone, whatever else subscribed to it before.
        let own = super::thread_rules::own_ticket_of(reference).map(str::to_owned);
        for ticket_id in thread.subscriptions.clone() {
            if own.as_ref().is_some_and(|own| *own != ticket_id) {
                continue;
            }
            let followed = thread
                .context
                .as_ref()
                .is_some_and(|c| c.contains(&ticket_id));
            let Some(ticket) = cx.tx.ticket(&ticket_id).await? else {
                continue;
            };
            // Only a person's message on a Thread of the ticket's own resumes an ended ticket; a
            // ticket a person stopped receives nothing, since its Mandate was withdrawn.
            let resumes = trigger.human
                && ticket.ended.as_ref().map(|e| e.by) != Some(TicketEndBy::Human)
                && !followed;
            if !live_ticket(&ticket) && !resumes {
                continue;
            }
            match reached.iter_mut().find(|(id, _, _)| *id == ticket_id) {
                Some(entry) => entry.2 = followed && entry.2,
                None => reached.push((ticket_id, ticket, followed)),
            }
        }
        cx.tx.save_thread(&thread).await?;
    }
    for (ticket_id, ticket, followed) in reached.iter_mut() {
        let input = TicketInput::Trigger {
            id: key(["input", ticket_id.as_str(), "trigger", trigger.id.as_str()]),
            trigger_id: trigger.id.clone(),
            at: at.clone(),
            note: None,
            human: trigger.human.then_some(true),
            context: (*followed).then_some(true),
            read_at: None,
            delivered_to: None,
            delivered_at: None,
        };
        tickets::add_input(cx, ticket, input).await?;
    }
    let fanout: Vec<String> = reached.iter().map(|(id, _, _)| id.clone()).collect();
    let context: Vec<String> = reached
        .iter()
        .filter(|(_, _, followed)| *followed)
        .map(|(id, _, _)| id.clone())
        .collect();
    cx.tx
        .save_disposition(
            &trigger.id,
            &TriggerDisposition {
                fanout: fanout.clone(),
                context: (!context.is_empty()).then_some(context),
                at,
                ..TriggerDisposition::default()
            },
        )
        .await?;
    // A person's message no ticket decides, and every bot's, goes to Eval.
    if cx.owners(&trigger).await?.is_empty() {
        cx.queue_for_eval(&trigger).await?;
    }
    for ticket_id in &fanout {
        tickets::wake(cx, ticket_id).await?;
    }
    cx.emit(DomainEvent::TriggerReceived {
        trigger_id: trigger.id.clone(),
        threads: keys,
    });
    Ok(ReceiveResult::Stored)
}
