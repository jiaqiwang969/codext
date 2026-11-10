//! The look at a driven session after its turn. A step that never started is withdrawn. A
//! finished turn's conclusion is recorded and handed to the Router, after the audit for an
//! anchored goal, which may stop the drive (deliverable, drift, no progress). A turn that failed
//! on its account goes again elsewhere (`switch.rs`); one that failed on the network goes again
//! once (`retry.rs`). A paused session's conclusion is kept quietly, so the Router can answer it
//! when it hands the session back.
//!
//! The hooks say which turn ended and how (`turns.rs`); the history has its conclusion. A look
//! that comes before the end reached the history fails, and the effect queue runs it again a
//! few seconds later.

use codex_protocol::ThreadId;

use super::DriveMode;
use super::DriveRecord;
use super::DriveService;
use super::DriveState;
use super::REPORT_CHARS;
use super::Report;
use super::accounts;
use super::drive_key;
use super::progress;
use super::report;
use super::retry;
use super::switch;
use super::turns;
use super::turns::Settled;
use super::withdraw_step;
use crate::cards::truncate_chars;
use crate::core::now_iso;
use crate::host::TurnBrief;

/// How many of a session's latest turns are searched for the one that ended.
const RECENT_TURNS: u32 = 5;
/// What the reason of a pause says once the step that was running when it paused has ended.
pub const DONE_WHILE_PAUSED: &str = "（这一步已做完，结论已记下）";

impl DriveService {
    /// Looks at the session after a turn: a step that never started is withdrawn; a finished
    /// turn's conclusion is recorded and handed to the Router, after the audit for an anchored
    /// goal, which may stop the drive (deliverable, drift, no progress).
    pub async fn step(&self, thread_id: &str) -> anyhow::Result<()> {
        let lock = self.inner.lock.lock().await;
        let Some(record) = self
            .inner
            .store
            .kv_get::<DriveRecord>(&drive_key(thread_id))
            .await?
        else {
            return Ok(());
        };
        if !record.watched() {
            return Ok(());
        }
        let thread = ThreadId::from_string(thread_id)?;
        if record.state == DriveState::Driving
            && let Some(step) = record.inflight
        {
            return self.check_stall(&record, thread, step).await;
        }
        let seen = self.turn_seen(thread_id);
        let turns = self.inner.host.session_turns(thread, RECENT_TURNS).await?;
        let wanted = seen.as_ref().and_then(|seen| seen.turn.as_deref());
        let found = match wanted {
            Some(id) => turns.iter().find(|turn| turn.id == id),
            None => turns.first(),
        };
        let ended = seen.as_ref().and_then(|seen| seen.ended);
        let turn = match found {
            Some(turn) => match turns::settle(turn, seen.as_ref()) {
                Settled::Ended(turn) => turn,
                Settled::Running => return Ok(()),
                Settled::Pending => {
                    anyhow::bail!("第 {} 步的结束还没写进会话记录，稍后再看", record.steps)
                }
            },
            // The history does not show the turn the hooks saw yet.
            None if ended.is_some_and(|at| at.elapsed() < turns::SETTLE_LIMIT) => {
                anyhow::bail!("第 {} 步的回合还没写进会话记录，稍后再看", record.steps)
            }
            None if ended.is_some() && record.state == DriveState::Driving => {
                let reason = format!(
                    "第 {} 步结束了，但会话记录里一直找不到这一轮，读不到它的结论",
                    record.steps
                );
                return self
                    .settle(&record, DriveState::Paused { reason }, None)
                    .await;
            }
            None => return Ok(()),
        };
        match &record.state {
            DriveState::Driving => {
                if record.started != Some(record.steps)
                    || record
                        .report
                        .as_ref()
                        .is_some_and(|report| report.turn == turn.id)
                {
                    return Ok(());
                }
                if turn.status != "completed" {
                    // The account could not carry the step: move the session and send it again.
                    if let Some(trouble) = accounts::trouble(&turn)
                        && self.inner.accounts.is_some()
                    {
                        let state = DriveState::Paused {
                            reason: format!(
                                "第 {} 步{}，{}",
                                record.steps,
                                trouble.describe(),
                                switch::SWITCHING
                            ),
                        };
                        self.change_locked(thread_id, |record| record.state = state)
                            .await?;
                        drop(lock);
                        return self.retry_elsewhere(record, thread, trouble).await;
                    }
                    // The network broke off: the same step goes again once, a little later.
                    if retry::transient(&turn) && record.retried != Some(record.steps) {
                        return self.retry_later(&record, &turn).await;
                    }
                    let reason = match &turn.error {
                        Some(error) => format!(
                            "第 {} 步没有正常结束（{}：{}）",
                            record.steps,
                            turn.status,
                            truncate_chars(error, 200)
                        ),
                        None => format!("第 {} 步没有正常结束（{}）", record.steps, turn.status),
                    };
                    return self
                        .settle(&record, DriveState::Paused { reason }, None)
                        .await;
                }
            }
            // The person typed into the session after the conclusion: a newer one replaces it.
            DriveState::Reported { .. } => {
                if turn.status != "completed"
                    || record
                        .report
                        .as_ref()
                        .is_some_and(|report| report.turn == turn.id)
                {
                    return Ok(());
                }
            }
            DriveState::Paused { reason } => {
                return self.keep_paused_conclusion(&record, &turn, reason).await;
            }
            DriveState::Deliverable { .. } | DriveState::NeedsDecision { .. } => return Ok(()),
        }
        let conclusion = report::conclusion(&turn);
        let mut report = Report {
            turn: turn.id.clone(),
            at: now_iso(),
            text: truncate_chars(&conclusion, REPORT_CHARS),
            audit: None,
        };
        let (state, digest, unchanged) = match record.mode {
            DriveMode::Simple => (report::reported(&record, &report.text), None, 0),
            DriveMode::Anchored => match self.inner.judge.audit(&record.project_home).await {
                // The judgment did not run: not the worker's doing, the drive goes on and the
                // next step's end is judged again.
                Err(err) => {
                    report.audit = Some(format!(
                        "这一步没判成（判定程序出错：{}），下一步结束再判",
                        truncate_chars(&format!("{err:#}"), 160)
                    ));
                    (
                        report::reported(&record, &report.text),
                        None,
                        record.unchanged,
                    )
                }
                Ok(audit) => {
                    report.audit = Some(audit.summary());
                    let digest = (audit.project_sha256.clone(), audit.anchor_sha256.clone());
                    // A step made progress when its records changed or anything else in its
                    // folder did: work that does not touch the records is still work.
                    let same_records = record.digest.as_ref() == Some(&digest);
                    let worked = || {
                        let folder = record.project_home.parent().unwrap_or(&record.project_home);
                        let since = record
                            .history
                            .iter()
                            .rev()
                            .find(|step| step.n == record.steps && !step.steered)
                            .map_or(record.since.as_str(), |step| step.at.as_str());
                        progress::changed_since(folder, since)
                    };
                    let unchanged = if same_records && !worked() {
                        record.unchanged + 1
                    } else {
                        0
                    };
                    (
                        report::judged(&record, &report.text, &audit, unchanged),
                        Some(digest),
                        unchanged,
                    )
                }
            },
        };
        let state = match state {
            DriveState::Reported { .. } if record.steps >= record.max_steps => DriveState::Paused {
                reason: format!("已推进 {} 步，达到上限；放宽要问用户", record.steps),
            },
            state => state,
        };
        let told = state.clone();
        let kept = report.clone();
        let changed = self
            .change_locked(thread_id, |record| {
                record.state = state;
                record.take_report(report);
                if let Some(digest) = digest {
                    record.digest = Some(digest);
                    record.unchanged = unchanged;
                }
                record.clone()
            })
            .await?;
        let record = changed.unwrap_or(DriveRecord {
            report: Some(kept),
            ..record
        });
        self.notify(&record, &told).await
    }

    /// A paused session's turn ended: its conclusion becomes the one the Router answers when it
    /// hands the session back, and the pause says the step is done. Nothing is told: the person
    /// or the Router paused it. The engine's own pauses while it moves or resends a step are left
    /// alone.
    async fn keep_paused_conclusion(
        &self,
        record: &DriveRecord,
        turn: &TurnBrief,
        reason: &str,
    ) -> anyhow::Result<()> {
        if turn.status != "completed"
            || reason.ends_with(switch::SWITCHING)
            || reason.ends_with(retry::RETRYING)
            || record
                .report
                .as_ref()
                .is_some_and(|report| report.turn == turn.id)
        {
            return Ok(());
        }
        let report = Report {
            turn: turn.id.clone(),
            at: now_iso(),
            text: truncate_chars(&report::conclusion(turn), REPORT_CHARS),
            audit: None,
        };
        self.change_locked(&record.thread_id, |record| {
            record.take_report(report);
            if let DriveState::Paused { reason } = &mut record.state
                && !reason.ends_with(DONE_WHILE_PAUSED)
            {
                reason.push_str(DONE_WHILE_PAUSED);
            }
        })
        .await
        .map(drop)
    }

    /// A step handed over and not started: wait, unless it has waited too long; then it is
    /// withdrawn and the drive pauses.
    pub(super) async fn check_stall(
        &self,
        record: &DriveRecord,
        thread: ThreadId,
        step: u32,
    ) -> anyhow::Result<()> {
        let handed = record
            .history
            .last()
            .and_then(|last| chrono::DateTime::parse_from_rfc3339(&last.at).ok())
            .map(|at| at.with_timezone(&chrono::Utc));
        let stalled = handed.is_some_and(|at| {
            chrono::Utc::now()
                .signed_duration_since(at)
                .to_std()
                .unwrap_or_default()
                >= self.inner.stall
        });
        if !stalled || self.session_running(thread).await? {
            return Ok(());
        }
        self.withdraw_steps(record).await?;
        let paused = DriveState::Paused {
            reason: format!("第 {step} 步交出后一直没有开始，已撤回"),
        };
        let state = paused.clone();
        self.change_locked(&record.thread_id, |record| {
            withdraw_step(record, step);
            record.state = state;
        })
        .await?;
        self.notify(record, &paused).await
    }

    /// Ends a look with a state and tells the Router, keeping the lock the caller holds.
    pub(super) async fn settle(
        &self,
        record: &DriveRecord,
        state: DriveState,
        report: Option<Report>,
    ) -> anyhow::Result<()> {
        let told = state.clone();
        let kept = report.clone();
        self.change_locked(&record.thread_id, |record| {
            record.state = state;
            if let Some(report) = report {
                record.take_report(report);
            }
        })
        .await?;
        let record = DriveRecord {
            report: kept.or_else(|| record.report.clone()),
            ..record.clone()
        };
        self.notify(&record, &told).await
    }
}
