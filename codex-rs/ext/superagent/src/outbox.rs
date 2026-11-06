//! The outbox: sends the engine's messages to their conversations (prototype `src/app/outbox.ts`
//! and the `reply` kind of `effects.ts`). The engine's replies record a receipt in `outbox` and
//! queue a `reply` effect keyed by the message id; this handler delivers it to the codext thread
//! the message is for, settles the receipt, and keeps a ticket's messages to one conversation in
//! order. A message is never sent twice: a settled receipt stops it, and a dropped one keeps its
//! id taken (design §4.6, §7).

use std::sync::Arc;

use codex_protocol::ThreadId;
use serde_json::Value;

use crate::core::ReportState;
use crate::core::TicketOutcome;
use crate::core::latest_report;
use crate::core::now_iso;
use crate::effects::EffectError;
use crate::effects::EffectRun;
use crate::effects::Effects;
use crate::effects::FAILURES_BEFORE_ERROR;
use crate::effects::HandlerOptions;
use crate::executor::codext_thread::session_key;
use crate::host::DeliveryMode;
use crate::host::SuperagentHost;
use crate::ids::key;
use crate::pm::PmThreads;
use crate::store::DropReason;
use crate::store::MessageKind;
use crate::store::Receipt;
use crate::store::ReplyStatus;
use crate::store::Settle;
use crate::store::Store;

pub const KIND_REPLY: &str = "reply";
/// The only source the outbox can write to: a codext thread (the intake's source).
const SOURCE: &str = "codext";

/// Delivers queued messages; one per app-server, registered on the effect queue by `install_outbox`.
pub struct Outbox {
    store: Store,
    host: Arc<dyn SuperagentHost>,
    pm_threads: PmThreads,
}

impl Outbox {
    pub fn new(store: Store, host: Arc<dyn SuperagentHost>, pm_threads: PmThreads) -> Self {
        Self {
            store,
            host,
            pm_threads,
        }
    }

    /// The relay format's prefix: what the message is, so the reader knows before the text. A
    /// reply is the ticket's open question, its result once a report is ready or the ticket ended
    /// with one, else an answer.
    pub async fn label(&self, receipt: &Receipt) -> anyhow::Result<&'static str> {
        match receipt.message.kind {
            MessageKind::Acknowledgement => Ok("[确认]"),
            MessageKind::Notice => Ok("[通知]"),
            MessageKind::Reply => {
                let Some(ticket_id) = &receipt.message.ticket_id else {
                    return Ok("[回答]");
                };
                let Some(ticket) = self.store.ticket(ticket_id).await? else {
                    return Ok("[回答]");
                };
                let attention = ticket
                    .attention
                    .as_ref()
                    .is_some_and(|a| key(["attention-reply", a.id.as_str()]) == receipt.message.id);
                if attention {
                    return Ok("[待决问题]");
                }
                let result = latest_report(&ticket).is_some_and(|r| r.state == ReportState::Ready)
                    || matches!(
                        ticket.ended.as_ref().map(|e| e.outcome),
                        Some(
                            TicketOutcome::Completed
                                | TicketOutcome::Merged
                                | TicketOutcome::Dropped
                        )
                    );
                Ok(if result { "[结果]" } else { "[回答]" })
            }
        }
    }

    /// Whether the conversation is one we know: a PM thread, a thread the engine has heard from, or
    /// a worker's thread.
    async fn known_thread(&self, receipt: &Receipt, thread: &str) -> anyhow::Result<bool> {
        if self.pm_threads.is_pm(thread) {
            return Ok(true);
        }
        let thread_key = format!("{}:{thread}", receipt.message.channel.source);
        if self.store.thread(&thread_key).await?.is_some() {
            return Ok(true);
        }
        let mut sessions = Vec::new();
        if let Some(ticket_id) = &receipt.message.ticket_id {
            sessions.push(session_key(ticket_id));
        }
        sessions.extend(self.store.kv_keys("codext-session:").await?);
        for session in sessions {
            if let Some(record) = self.store.kv_get::<Value>(&session).await?
                && record["thread_id"].as_str() == Some(thread)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn settle(&self, receipt: &Receipt, outcome: Settle) -> anyhow::Result<()> {
        let message = receipt.message.clone();
        self.store
            .atomic(async |tx| tx.receipt_settle(&message, outcome, &now_iso()).await)
            .await
    }

    /// Sends the message a receipt is for, once: nothing for a settled receipt; `Later` while an
    /// earlier message of the same ticket to the same conversation waits with fewer than three
    /// failures; dropped (`ChannelGone`) when the thread is unknown or cannot be loaded or
    /// written; a thread that loads but refuses the message now is tried again later.
    pub async fn send(&self, id: &str) -> Result<(), EffectError> {
        let Some(receipt) = self.store.receipt(id).await? else {
            return Ok(());
        };
        if receipt.status != ReplyStatus::Pending {
            return Ok(());
        }
        if self.store.receipt_behind(id, FAILURES_BEFORE_ERROR).await? {
            return Err(EffectError::Later(
                "an earlier message of the ticket to this conversation waits".to_owned(),
            ));
        }
        if receipt.message.channel.source != SOURCE {
            tracing::info!(
                "superagent: dropping message {id}: no way to write to source {}",
                receipt.message.channel.source
            );
            self.settle(&receipt, Settle::Dropped(DropReason::ChannelIgnored))
                .await?;
            return Ok(());
        }
        let thread_id = receipt.message.channel.thread.clone();
        let thread = match ThreadId::from_string(&thread_id) {
            Ok(thread) if self.known_thread(&receipt, &thread_id).await? => thread,
            _ => {
                tracing::info!(
                    "superagent: dropping message {id}: conversation {thread_id} is unknown"
                );
                self.settle(&receipt, Settle::Dropped(DropReason::ChannelGone))
                    .await?;
                return Ok(());
            }
        };
        let text = format!("{} {}", self.label(&receipt).await?, receipt.message.text);
        let resumed = self.host.resume_thread(thread, None).await;
        if let Err(err) = &resumed {
            tracing::warn!("superagent: resume of {thread} before message {id} failed: {err:#}");
        }
        match self.host.deliver(thread, text, DeliveryMode::Queue).await {
            Ok(()) => {
                self.settle(&receipt, Settle::Sent).await?;
                Ok(())
            }
            Err(err) if resumed.is_err() => {
                tracing::warn!(
                    "superagent: dropping message {id}: {thread} neither loads nor takes it: {err:#}"
                );
                self.settle(&receipt, Settle::Dropped(DropReason::ChannelGone))
                    .await?;
                Ok(())
            }
            Err(err) => Err(EffectError::Failed(format!("{err:#}"))),
        }
    }
}

/// Registers the `reply` handler: one message at a time, retried from 5 seconds doubling to 300.
/// The queue's `ordered` option is not used: it would hold every message behind a failing one,
/// while the outbox rule is per ticket and conversation, released after three failures
/// (`Store::receipt_behind`), so messages of other tickets keep flowing.
pub async fn install_outbox(
    effects: Effects,
    store: Store,
    host: Arc<dyn SuperagentHost>,
    pm_threads: PmThreads,
) -> anyhow::Result<Arc<Outbox>> {
    let outbox = Arc::new(Outbox::new(store, host, pm_threads));
    let run: EffectRun = {
        let outbox = Arc::clone(&outbox);
        Arc::new(move |_payload: Value, id: String| {
            let outbox = Arc::clone(&outbox);
            Box::pin(async move { outbox.send(&id).await })
        })
    };
    effects
        .handle(
            KIND_REPLY,
            run,
            HandlerOptions::default()
                .limit(|| Ok(1))
                .retry(5_000, 300_000),
        )
        .await?;
    Ok(outbox)
}
