//! What the turn hooks of a driven session said (`lifecycle.rs`), kept in memory: which turn
//! runs, when it ended and how. The end of a turn reaches the session's history a moment after
//! its hook. Until it does, the history shows the turn still running or, once the session is no
//! longer running, interrupted and without its error. The look after a turn (`look.rs`) waits for
//! the history to settle and fills in what the hooks know, so a turn whose error hook reported a
//! usage limit is a failed turn on its account, whatever the history shows meanwhile.

use std::collections::HashMap;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use codex_protocol::ThreadId;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::TurnAbortReason;

use super::DriveService;
use super::DriveState;
use crate::host::TurnBrief;

/// How long the look after a turn waits for the turn's end to reach the history before it goes
/// by what the hooks said.
pub(super) const SETTLE_LIMIT: Duration = Duration::from_secs(60);

/// What the hooks said about a driven session's latest turn.
#[derive(Clone, Debug, Default)]
pub(super) struct TurnSeen {
    /// The turn the start hook saw; `None` when the host did not say.
    pub(super) turn: Option<String>,
    /// When it started.
    pub(super) started: Option<Instant>,
    /// When its stop or abort hook ran.
    pub(super) ended: Option<Instant>,
    /// The error its error hook reported.
    pub(super) error: Option<SeenError>,
    /// Why it was aborted, when it was.
    pub(super) aborted: Option<TurnAbortReason>,
}

/// A turn's error as its error hook reported it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SeenError {
    /// The error's kind as the session's history names it (`usageLimitExceeded`).
    pub(super) kind: &'static str,
    /// What the person saw.
    pub(super) message: String,
}

/// The history's view of a turn, against what the hooks said.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Settled {
    /// The turn has ended; this is how, with what the hooks know filled in.
    Ended(TurnBrief),
    /// The turn runs, and no hook said otherwise: its end looks again.
    Running,
    /// A hook said the turn ended, and the history does not show it yet.
    Pending,
}

/// The kind of a core error as the session's history names it.
pub(super) fn kind_name(error: &CodexErrorInfo) -> &'static str {
    match error {
        CodexErrorInfo::ContextWindowExceeded => "contextWindowExceeded",
        CodexErrorInfo::SessionBudgetExceeded => "sessionBudgetExceeded",
        CodexErrorInfo::UsageLimitExceeded => "usageLimitExceeded",
        CodexErrorInfo::RateLimitExceeded => "rateLimitExceeded",
        CodexErrorInfo::FlexUnavailable => "flexUnavailable",
        CodexErrorInfo::ServerOverloaded => "serverOverloaded",
        CodexErrorInfo::ModelNotFound => "modelNotFound",
        CodexErrorInfo::PolicyCapacityUnavailable => "policyCapacityUnavailable",
        CodexErrorInfo::CyberPolicy => "cyberPolicy",
        CodexErrorInfo::BioPolicy => "bioPolicy",
        CodexErrorInfo::MisalignmentPolicyViolation => "misalignmentPolicyViolation",
        CodexErrorInfo::TooManyDenials => "tooManyDenials",
        CodexErrorInfo::HttpConnectionFailed { .. } => "httpConnectionFailed",
        CodexErrorInfo::ResponseStreamConnectionFailed { .. } => "responseStreamConnectionFailed",
        CodexErrorInfo::InternalServerError => "internalServerError",
        CodexErrorInfo::Unauthorized => "unauthorized",
        CodexErrorInfo::BadRequest => "badRequest",
        CodexErrorInfo::InvalidPrompt => "invalidPrompt",
        CodexErrorInfo::SandboxError => "sandboxError",
        CodexErrorInfo::ResponseStreamDisconnected { .. } => "responseStreamDisconnected",
        CodexErrorInfo::ResponseTooManyFailedAttempts { .. } => "responseTooManyFailedAttempts",
        CodexErrorInfo::ActiveTurnNotSteerable { .. } => "activeTurnNotSteerable",
        CodexErrorInfo::ThreadRollbackFailed => "threadRollbackFailed",
        CodexErrorInfo::Other => "other",
    }
}

/// How `turn`, as the history shows it, ended, given what the hooks said about it (`seen`, for
/// the same turn) and how long ago its end hook ran. A history that still shows the turn running,
/// or interrupted without an error after an ordinary end, has not caught up yet: `Pending` until
/// `SETTLE_LIMIT`, then the hooks decide.
pub(super) fn settle(turn: &TurnBrief, seen: Option<&TurnSeen>) -> Settled {
    let Some(seen) = seen.filter(|seen| seen.ended.is_some()) else {
        return if turn.status == "inProgress" {
            Settled::Running
        } else {
            Settled::Ended(turn.clone())
        };
    };
    let with_error = |status: &str| {
        let mut ended = TurnBrief {
            status: status.to_owned(),
            ..turn.clone()
        };
        if ended.error.is_none()
            && let Some(error) = &seen.error
        {
            ended.error = Some(error.message.clone());
            ended.error_kind = Some(error.kind.to_owned());
        }
        Settled::Ended(ended)
    };
    let caught_up = match turn.status.as_str() {
        "completed" => return Settled::Ended(turn.clone()),
        "failed" => return with_error("failed"),
        "interrupted" => match (&seen.aborted, &seen.error) {
            (Some(_), _) => return Settled::Ended(turn.clone()),
            // The session stopped running before the end reached the history: the error the
            // hook reported is how the turn ended.
            (None, Some(_)) => return with_error("failed"),
            (None, None) => false,
        },
        _ => false,
    };
    let waited = seen
        .ended
        .is_some_and(|ended| ended.elapsed() >= SETTLE_LIMIT);
    if caught_up || !waited {
        return Settled::Pending;
    }
    // The history never caught up: the hooks say how the turn ended.
    match (&seen.aborted, &seen.error) {
        (Some(TurnAbortReason::Interrupted), _) => Settled::Ended(TurnBrief {
            status: "interrupted".to_owned(),
            ..turn.clone()
        }),
        (_, Some(_)) => with_error("failed"),
        (Some(_), None) | (None, None) => Settled::Ended(TurnBrief {
            status: "completed".to_owned(),
            ..turn.clone()
        }),
    }
}

impl DriveService {
    pub(super) fn seen(&self) -> MutexGuard<'_, HashMap<String, TurnSeen>> {
        self.inner
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// What the hooks said about the session's latest turn.
    pub(super) fn turn_seen(&self, thread_id: &str) -> Option<TurnSeen> {
        self.seen().get(thread_id).cloned()
    }

    /// A turn started on the session (its start hook, with the turn's id when the host gives
    /// it): the step in flight reached it.
    pub async fn turn_began(&self, thread_id: &str, turn: Option<&str>) -> anyhow::Result<()> {
        let driven = self
            .change(thread_id, |record| {
                if let Some(step) = record.inflight.take() {
                    record.started = Some(step);
                }
            })
            .await?
            .is_some();
        if driven {
            self.seen().insert(
                thread_id.to_owned(),
                TurnSeen {
                    turn: turn.map(str::to_owned),
                    started: Some(Instant::now()),
                    ..TurnSeen::default()
                },
            );
        }
        Ok(())
    }

    /// A turn started on the session: the step in flight reached it.
    pub async fn turn_started(&self, thread_id: &str) -> anyhow::Result<()> {
        self.turn_began(thread_id, /*turn*/ None).await
    }

    /// A step's words joined `turn`, which ran already: its end answers the step.
    pub(super) fn adopt_turn(&self, thread_id: &str, turn: &str) {
        let mut seen = self.seen();
        let current = seen.entry(thread_id.to_owned()).or_default();
        if current.turn.as_deref() != Some(turn) {
            *current = TurnSeen {
                turn: Some(turn.to_owned()),
                started: Some(Instant::now()),
                ..TurnSeen::default()
            };
        }
    }

    /// The error hook of a driven session's turn: kept for the look after the turn, which may
    /// read the history before the error reached it.
    pub fn turn_failed(&self, thread_id: &str, turn: &str, error: &CodexErrorInfo, message: &str) {
        let mut seen = self.seen();
        let Some(seen) = seen.get_mut(thread_id) else {
            return;
        };
        if seen.turn.as_deref().is_some_and(|current| current != turn) {
            return;
        }
        seen.turn.get_or_insert_with(|| turn.to_owned());
        seen.error = Some(SeenError {
            kind: kind_name(error),
            message: message.to_owned(),
        });
    }

    /// The end of the session's turn, as its stop or abort hook says.
    pub(super) fn turn_ended(&self, thread_id: &str, aborted: Option<TurnAbortReason>) {
        if let Some(seen) = self.seen().get_mut(thread_id) {
            seen.ended = Some(Instant::now());
            seen.aborted = aborted;
        }
    }

    /// A turn ended: its conclusion is read next. A paused session's turns are read too, so the
    /// conclusion of a step that was still running when the drive paused is kept.
    pub async fn turn_stopped(&self, thread_id: &str) -> anyhow::Result<()> {
        if self
            .turn_seen(thread_id)
            .is_some_and(|seen| seen.ended.is_none())
        {
            self.turn_ended(thread_id, /*aborted*/ None);
        }
        match self.record(thread_id).await? {
            Some(record) if record.watched() => self.schedule(thread_id).await?,
            Some(_) => {}
            // A drafting session's turn ended: its session's draft reads the answer.
            None => {
                if let Some(session) = self.drafted_session(thread_id).await? {
                    self.inner
                        .effects
                        .enqueue_now(super::KIND_DRAFT, &session, &serde_json::json!({}))
                        .await?;
                }
            }
        }
        Ok(())
    }

    /// A driven turn was stopped for another reason than an interruption (a replacing turn, a
    /// review that ended, a budget stop): it ends in its own judgment.
    pub async fn turn_aborted(
        &self,
        thread_id: &str,
        reason: TurnAbortReason,
    ) -> anyhow::Result<()> {
        self.turn_ended(thread_id, Some(reason));
        self.turn_stopped(thread_id).await
    }

    /// The person interrupted a driven turn: that is taking the session over.
    pub async fn turn_interrupted(&self, thread_id: &str) -> anyhow::Result<()> {
        self.turn_ended(thread_id, Some(TurnAbortReason::Interrupted));
        self.change(thread_id, |record| {
            if record.state == DriveState::Driving {
                record.state = DriveState::Paused {
                    reason: "你中断了这一步，推进已暂停；在 Router 里说交回即可继续".to_owned(),
                };
            }
        })
        .await
        .map(drop)
    }

    /// Whether a turn runs on the session now: the history shows one running, and no hook said
    /// it ended.
    pub(super) async fn session_running(&self, thread: ThreadId) -> anyhow::Result<bool> {
        let latest = self.inner.host.session_turns(thread, 1).await?;
        let Some(turn) = latest.first() else {
            return Ok(false);
        };
        if turn.status != "inProgress" {
            return Ok(false);
        }
        let ended = self.turn_seen(&thread.to_string()).is_some_and(|seen| {
            seen.ended.is_some() && seen.turn.as_deref().is_none_or(|ended| ended == turn.id)
        });
        Ok(!ended)
    }
}
