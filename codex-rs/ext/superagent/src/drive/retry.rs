//! A step whose turn failed on the network (the stream broke off, the account's workspace could
//! not be routed, the server failed) goes again once on the same account after a short wait. A
//! second failure of the same step pauses the drive with its error, as any other failure does.
//! The pause in between is the engine's own, so a Router that speaks to the session meanwhile
//! takes it over and the resend does nothing.

use std::sync::Arc;
use std::time::Duration;

use codex_protocol::ThreadId;
use serde_json::Value;
use serde_json::json;

use super::DriveRecord;
use super::DriveService;
use super::DriveState;
use super::switch::step_text;
use crate::cards::truncate_chars;
use crate::core::now_iso;
use crate::effects::EffectError;
use crate::effects::Effects;
use crate::effects::HandlerOptions;
use crate::host::TurnBrief;

/// The effect kind that sends a step again after its turn failed on the network.
pub const KIND_RETRY: &str = "drive-step-retry";
/// How the pause before the resend ends; only such a pause is resumed by it.
pub const RETRYING: &str = "稍后自动重发这一步";
/// How long the resend waits.
pub const RETRY_AFTER: Duration = Duration::from_secs(45);

/// The error kinds of a turn that broke off on the way to the model rather than on the work.
const TRANSIENT_KINDS: &[&str] = &[
    "httpConnectionFailed",
    "responseStreamConnectionFailed",
    "responseStreamDisconnected",
    "responseTooManyFailedAttempts",
    "internalServerError",
];
/// What the messages of such failures say, when their kind is a generic one.
const TRANSIENT_WORDS: &[&str] = &[
    "stream disconnected",
    "error decoding response body",
    "workspace routing discovery",
    "error sending request",
    "connection reset",
    "connection closed",
    "broken pipe",
];

/// Whether a failed turn broke off on the network, so the same step may simply go again.
pub(super) fn transient(turn: &TurnBrief) -> bool {
    if turn
        .error_kind
        .as_deref()
        .is_some_and(|kind| TRANSIENT_KINDS.contains(&kind))
    {
        return true;
    }
    let message = turn.error.as_deref().unwrap_or_default().to_lowercase();
    TRANSIENT_WORDS.iter().any(|words| message.contains(words))
}

impl DriveService {
    /// Step `n` broke off on the network: the drive pauses and the step goes again once, after
    /// `RETRY_AFTER`, on the same account. Called under the drive's lock.
    pub(super) async fn retry_later(
        &self,
        record: &DriveRecord,
        turn: &TurnBrief,
    ) -> anyhow::Result<()> {
        let n = record.steps;
        let error = truncate_chars(turn.error.as_deref().unwrap_or("网络中断"), 120);
        let state = DriveState::Paused {
            reason: format!("第 {n} 步遇到网络问题（{error}），{RETRYING}"),
        };
        self.change_locked(&record.thread_id, |record| {
            record.state = state;
            record.retried = Some(n);
        })
        .await?;
        self.inner
            .effects
            .enqueue_after(
                KIND_RETRY,
                &record.thread_id,
                &json!({ "step": n }),
                self.inner.retry_after,
            )
            .await
    }

    /// The resend of step `n` (the effect `KIND_RETRY` runs it), unless the Router has spoken to
    /// the session since.
    pub async fn resend_step(&self, thread_id: &str, n: u32) -> anyhow::Result<()> {
        let thread = ThreadId::from_string(thread_id)?;
        let changed = self
            .change(thread_id, |record| {
                let ours = matches!(&record.state, DriveState::Paused { reason } if reason.ends_with(RETRYING))
                    && record.steps == n;
                if !ours {
                    return None;
                }
                let text = step_text(record)?;
                record.state = DriveState::Driving;
                record.inflight = Some(n);
                if let Some(step) = record
                    .history
                    .iter_mut()
                    .rev()
                    .find(|step| step.n == n && !step.steered)
                {
                    step.at = now_iso();
                }
                Some((record.title.clone(), text))
            })
            .await?;
        let Some(Some((title, text))) = changed else {
            return Ok(());
        };
        tracing::info!("superagent drive: step {n} of {title} goes again after a network failure");
        self.hand_over(thread_id, thread, &title, n, text).await
    }
}

/// Registers the handler of the resends.
pub(super) async fn install_retry_handler(
    service: &DriveService,
    effects: &Effects,
) -> anyhow::Result<()> {
    let resender = service.clone();
    effects
        .handle(
            KIND_RETRY,
            Arc::new(move |payload: Value, thread_id| {
                let resender = resender.clone();
                Box::pin(async move {
                    let Some(n) = payload
                        .get("step")
                        .and_then(Value::as_u64)
                        .and_then(|n| u32::try_from(n).ok())
                    else {
                        return Ok(());
                    };
                    resender
                        .resend_step(&thread_id, n)
                        .await
                        .map_err(|err| EffectError::Later(format!("{err:#}")))
                })
            }),
            HandlerOptions::default().retry(10_000, 120_000),
        )
        .await
}
