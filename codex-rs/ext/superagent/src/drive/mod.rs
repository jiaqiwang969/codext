//! The drive loop (command-center architecture §5–§6; daemon integration P2): the engine drives
//! a session the Router took under drive, toward a goal anchored in the session's Project
//! (`<project>/.engineering`). After each of the session's turns it runs the engineering
//! ontology's `route-audit` and then either sends the next step, written from the anchor and the
//! audit, or stops and tells the Router: deliverable, needs a decision, or paused.
//!
//! One record per driven session lives in the ledger's `kv` table (`drive:<thread id>`), so a
//! restart neither repeats nor loses a step: a step is recorded as in flight before it is handed
//! over and cleared when its turn starts; a recovered step that never started is not replayed.

mod anchor;
mod audit;
mod draft;
mod feedback;
mod lifecycle;

use std::path::PathBuf;
use std::sync::Arc;

use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

pub use anchor::Goal;
pub use audit::Anchor;
pub use audit::Audit;
pub use audit::Judge;
pub use audit::Oev2Judge;
pub use audit::Workspace;
pub use audit::workspace;
pub use draft::Focus;
pub use draft::STEP_LABEL;
pub use draft::compose;
pub use draft::focus;
pub use feedback::Feedback;
pub use lifecycle::DriveLifecycle;

use crate::core::now_iso;
use crate::effects::EffectError;
use crate::effects::Effects;
use crate::effects::HandlerOptions;
use crate::host::SuperagentHost;
use crate::store::Store;

pub const KEY_PREFIX: &str = "drive:";
pub const KIND_STEP: &str = "drive-step";
/// The effect kind that delivers a notice to the Router (`pm.rs`).
const NOTICE: &str = "notice";
pub const DEFAULT_MAX_STEPS: u32 = 20;
/// How long a handed-over step may wait for its turn to start.
pub const STALL: std::time::Duration = std::time::Duration::from_secs(120);
/// Judgments in a row that found the Project and the anchor unchanged before the drive stops.
pub const UNCHANGED_LIMIT: u32 = 2;
const HISTORY: usize = 20;
const NOTES: usize = 10;

pub fn drive_key(thread_id: &str) -> String {
    format!("{KEY_PREFIX}{thread_id}")
}

/// Takes back a step that never reached its session: it no longer counts, and the next step
/// sent is this one again.
fn withdraw_step(record: &mut DriveRecord, step: u32) {
    record.inflight = None;
    if record.steps == step {
        record.steps -= 1;
        record.history.pop();
    }
}

/// Who moves a driven session now.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DriveState {
    /// The engine judges each turn's end and sends the next step.
    Driving,
    /// The person took the session over, or the engine paused it; nothing is sent until resumed.
    Paused { reason: String },
    /// Every acceptance item is verified; the person accepts the delivery.
    Deliverable { summary: String },
    /// The engine stopped and needs the person: drift, no progress, or an audit it cannot run.
    NeedsDecision { reason: String },
}

/// One step the engine sent: when, on what judgment, for which item, and the prompt itself.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Step {
    pub n: u32,
    pub at: String,
    pub audit: String,
    pub focus: String,
    pub prompt: String,
}

/// A driven session.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DriveRecord {
    pub thread_id: String,
    pub title: String,
    /// The goal's Project workspace, `<project>/.engineering`.
    pub project_home: PathBuf,
    pub state: DriveState,
    pub steps: u32,
    pub max_steps: u32,
    /// Judgments in a row that found the same Project and anchor digests.
    pub unchanged: u32,
    /// (Project, anchor) digests of the last judgment.
    pub digest: Option<(String, String)>,
    /// What the person typed into the session since the last step.
    pub notes: Vec<String>,
    /// The step handed over whose turn has not started yet.
    pub inflight: Option<u32>,
    /// The last step whose turn did start.
    #[serde(default)]
    pub started: Option<u32>,
    /// The latest steps, oldest first.
    pub history: Vec<Step>,
    pub since: String,
    pub updated: String,
}

impl DriveRecord {
    /// One line for the Router: the goal, the state and the step count.
    pub fn line(&self) -> String {
        let state = match &self.state {
            DriveState::Driving => "推进中".to_owned(),
            DriveState::Paused { reason } => format!("已暂停：{reason}"),
            DriveState::Deliverable { summary } => format!("可交付，等你验收：{summary}"),
            DriveState::NeedsDecision { reason } => format!("需要你决定：{reason}"),
        };
        format!(
            "{} | {} | 第 {}/{} 步 | {state}",
            self.thread_id, self.title, self.steps, self.max_steps
        )
    }
}

/// What a drive starts from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveStart {
    /// The goal's Project workspace, `<project>/.engineering`.
    pub project_home: PathBuf,
    pub title: String,
    /// The most steps to send before pausing; the default when absent.
    pub max_steps: Option<u32>,
    /// The goal the person confirmed, anchored when the project has no anchor yet; an existing
    /// anchor is used as it is.
    pub goal: Option<Goal>,
}

/// The drive side of the engine.
#[derive(Clone)]
pub struct DriveService {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    effects: Effects,
    host: Arc<dyn SuperagentHost>,
    judge: Arc<dyn Judge>,
    /// How long a handed-over step may wait for its turn before the drive pauses.
    stall: std::time::Duration,
    /// Serializes changes to the records, so a turn's end and a tool call never interleave.
    lock: tokio::sync::Mutex<()>,
}

impl DriveService {
    /// Builds the service and registers its step handler; call before the effects start.
    pub async fn install(
        store: Store,
        effects: Effects,
        host: Arc<dyn SuperagentHost>,
        judge: Arc<dyn Judge>,
        stall: std::time::Duration,
    ) -> anyhow::Result<Self> {
        let service = Self {
            inner: Arc::new(Inner {
                stall,
                store,
                effects: effects.clone(),
                host,
                judge,
                lock: tokio::sync::Mutex::new(()),
            }),
        };
        let runner = service.clone();
        effects
            .handle(
                KIND_STEP,
                Arc::new(move |_payload, thread_id| {
                    let runner = runner.clone();
                    Box::pin(async move {
                        runner
                            .step(&thread_id)
                            .await
                            .map_err(|err| EffectError::Later(format!("{err:#}")))
                    })
                }),
                HandlerOptions::default()
                    .limit(|| Ok(2))
                    .retry(5_000, 60_000),
            )
            .await?;
        Ok(service)
    }

    pub async fn record(&self, thread_id: &str) -> anyhow::Result<Option<DriveRecord>> {
        self.inner.store.kv_get(&drive_key(thread_id)).await
    }

    /// Every driven session, in key order.
    pub async fn records(&self) -> anyhow::Result<Vec<DriveRecord>> {
        let mut records = Vec::new();
        for key in self.inner.store.kv_keys(KEY_PREFIX).await? {
            if let Some(record) = self.inner.store.kv_get::<DriveRecord>(&key).await? {
                records.push(record);
            }
        }
        Ok(records)
    }

    /// Whether the engine drives the session now (paused and stopped sessions are the person's).
    pub async fn drives(&self, thread_id: &str) -> anyhow::Result<bool> {
        Ok(self
            .record(thread_id)
            .await?
            .is_some_and(|record| record.state == DriveState::Driving))
    }

    /// Changes a record under the lock and in one transaction; `None` when there is none.
    async fn change<R>(
        &self,
        thread_id: &str,
        change: impl FnOnce(&mut DriveRecord) -> R,
    ) -> anyhow::Result<Option<R>> {
        let _lock = self.inner.lock.lock().await;
        self.change_locked(thread_id, change).await
    }

    async fn change_locked<R>(
        &self,
        thread_id: &str,
        change: impl FnOnce(&mut DriveRecord) -> R,
    ) -> anyhow::Result<Option<R>> {
        let key = drive_key(thread_id);
        self.inner
            .store
            .atomic(async |tx| {
                let Some(mut record) = tx.kv_get::<DriveRecord>(&key).await? else {
                    return Ok(None);
                };
                let result = change(&mut record);
                record.updated = now_iso();
                tx.kv_set(&key, &record).await?;
                Ok(Some(result))
            })
            .await
    }

    async fn schedule(&self, thread_id: &str) -> anyhow::Result<()> {
        self.inner
            .effects
            .enqueue_now(KIND_STEP, thread_id, &json!({}))
            .await
    }

    /// Takes a session under drive toward the goal anchored in `project_home`; the first step is
    /// judged at once (or when the session's running turn ends).
    pub async fn start(&self, thread_id: &str, start: DriveStart) -> anyhow::Result<DriveRecord> {
        ThreadId::from_string(thread_id)
            .map_err(|err| anyhow::anyhow!("会话 id {thread_id} 无效：{err}"))?;
        let DriveStart {
            project_home,
            title,
            max_steps,
            goal,
        } = start;
        // A project without an anchor gets one from the goal the person confirmed.
        if let Some(goal) = &goal
            && workspace(&project_home).is_err()
        {
            anchor::create(&project_home, goal)?;
        }
        workspace(&project_home)?;
        let now = now_iso();
        let record = DriveRecord {
            thread_id: thread_id.to_owned(),
            title,
            project_home,
            state: DriveState::Driving,
            steps: 0,
            max_steps: max_steps.filter(|n| *n > 0).unwrap_or(DEFAULT_MAX_STEPS),
            unchanged: 0,
            digest: None,
            notes: Vec::new(),
            inflight: None,
            started: None,
            history: Vec::new(),
            since: now.clone(),
            updated: now,
        };
        let key = drive_key(thread_id);
        {
            let _lock = self.inner.lock.lock().await;
            self.inner
                .store
                .atomic(async |tx| {
                    if let Some(existing) = tx.kv_get::<DriveRecord>(&key).await? {
                        anyhow::bail!(
                            "会话 {thread_id} 已经在推进记录里（{}）；用 resume 交回或 stop 释放",
                            existing.line()
                        );
                    }
                    tx.kv_set(&key, &record).await
                })
                .await?;
        }
        self.schedule(thread_id).await?;
        Ok(record)
    }

    /// The person takes the session over: no step is sent until `resume`.
    pub async fn pause(
        &self,
        thread_id: &str,
        reason: &str,
    ) -> anyhow::Result<Option<DriveRecord>> {
        self.change(thread_id, |record| {
            record.state = DriveState::Paused {
                reason: reason.to_owned(),
            };
            record.clone()
        })
        .await
    }

    /// Hands the session back: the next judgment reads the Project and the person's notes again.
    pub async fn resume(&self, thread_id: &str) -> anyhow::Result<Option<DriveRecord>> {
        let record = self
            .change(thread_id, |record| {
                record.state = DriveState::Driving;
                record.unchanged = 0;
                record.clone()
            })
            .await?;
        if record.is_some() {
            self.schedule(thread_id).await?;
        }
        Ok(record)
    }

    /// The person's acceptance feedback: written into the anchor (`feedback.rs`), kept as a note
    /// for the next step, and the drive goes on (from a stop too: the person has decided).
    pub async fn feedback(
        &self,
        thread_id: &str,
        feedback: Feedback,
    ) -> anyhow::Result<Option<DriveRecord>> {
        let Some(record) = self.record(thread_id).await? else {
            return Ok(None);
        };
        let request = feedback::record(&record.project_home, &feedback)?;
        let list = |ids: &[String]| {
            if ids.is_empty() {
                "无".to_owned()
            } else {
                ids.join("、")
            }
        };
        let note = format!(
            "用户亲自验收（已记入锚定记录 {request}）：通过 {}；没通过 {}。原话：{}。通过的项把用户的验收登记为证据和决定后关闭；没通过的项按原话修好并重新验证。",
            list(&feedback.passed),
            list(&feedback.failed),
            feedback.text.trim()
        );
        let record = self
            .change(thread_id, |record| {
                record.notes.push(note);
                if record.notes.len() > NOTES {
                    record.notes.remove(0);
                }
                record.state = DriveState::Driving;
                record.unchanged = 0;
                record.clone()
            })
            .await?;
        if record.is_some() {
            self.schedule(thread_id).await?;
        }
        Ok(record)
    }

    /// Raises (or lowers) the most steps the drive may send; never below the steps sent.
    pub async fn set_max_steps(&self, thread_id: &str, max_steps: u32) -> anyhow::Result<()> {
        self.change(thread_id, |record| {
            record.max_steps = max_steps.max(record.steps);
        })
        .await
        .map(drop)
    }

    /// Releases the session: the record goes and so do the steps still waiting in its queue.
    pub async fn stop(&self, thread_id: &str) -> anyhow::Result<Option<DriveRecord>> {
        let record = {
            let _lock = self.inner.lock.lock().await;
            let key = drive_key(thread_id);
            self.inner
                .store
                .atomic(async |tx| {
                    let record = tx.kv_get::<DriveRecord>(&key).await?;
                    tx.kv_delete(&key).await?;
                    Ok(record)
                })
                .await?
        };
        if record.is_some() {
            self.withdraw_steps(thread_id).await?;
        }
        Ok(record)
    }

    /// Removes the engine's steps still waiting in the session's persistent queue.
    async fn withdraw_steps(&self, thread_id: &str) -> anyhow::Result<usize> {
        let thread = ThreadId::from_string(thread_id)?;
        let mut withdrawn = 0;
        for (item, text) in self.inner.host.queued(thread).await? {
            if text.trim_start().starts_with(STEP_LABEL)
                && self.inner.host.withdraw_queued(thread, item).await?
            {
                withdrawn += 1;
            }
        }
        Ok(withdrawn)
    }

    /// A turn started on the session: the step in flight reached it.
    pub async fn turn_started(&self, thread_id: &str) -> anyhow::Result<()> {
        self.change(thread_id, |record| {
            if let Some(step) = record.inflight.take() {
                record.started = Some(step);
            }
        })
        .await
        .map(drop)
    }

    /// A turn ended: a driven session is judged next.
    pub async fn turn_stopped(&self, thread_id: &str) -> anyhow::Result<()> {
        if self.drives(thread_id).await? {
            self.schedule(thread_id).await?;
        }
        Ok(())
    }

    /// The person interrupted a driven turn: that is taking the session over.
    pub async fn turn_interrupted(&self, thread_id: &str) -> anyhow::Result<()> {
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

    /// What the person typed into a driven session; the next step reminds the worker of it.
    pub async fn person_input(&self, thread_id: &str, text: &str) -> anyhow::Result<()> {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return Ok(());
        }
        self.change(thread_id, |record| {
            record.notes.push(text);
            if record.notes.len() > NOTES {
                record.notes.remove(0);
            }
        })
        .await
        .map(drop)
    }

    /// After a restart: a step handed over but never started is not replayed. Its queued text
    /// is withdrawn and the drive pauses; every other driven session is judged again.
    pub async fn recover(&self) -> anyhow::Result<()> {
        for record in self.records().await? {
            if record.state != DriveState::Driving {
                continue;
            }
            match record.inflight {
                Some(step) => {
                    self.withdraw_steps(&record.thread_id).await?;
                    let reason =
                        format!("服务重启时第 {step} 步已交出但没有看到开始，没有自动重放");
                    let state = DriveState::Paused {
                        reason: reason.clone(),
                    };
                    self.change(&record.thread_id, |record| {
                        withdraw_step(record, step);
                        record.state = state;
                    })
                    .await?;
                    self.notify(
                        &record.thread_id,
                        &record.title,
                        &DriveState::Paused { reason },
                    )
                    .await?;
                }
                None => self.schedule(&record.thread_id).await?,
            }
        }
        Ok(())
    }

    /// Judges the session's last turn and sends the next step, or stops and says why.
    pub async fn step(&self, thread_id: &str) -> anyhow::Result<()> {
        // The lock covers the judgment and the write-ahead record, not the hand-over: delivering
        // starts the session's turn, whose start and input hooks change this record too.
        let lock = self.inner.lock.lock().await;
        let Some(record) = self
            .inner
            .store
            .kv_get::<DriveRecord>(&drive_key(thread_id))
            .await?
        else {
            return Ok(());
        };
        if record.state != DriveState::Driving {
            return Ok(());
        }
        let thread = ThreadId::from_string(thread_id)?;
        if let Some(step) = record.inflight {
            // Handed over and not started: wait, unless it has waited too long.
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
            if !stalled {
                return Ok(());
            }
            let latest = self.inner.host.session_turns(thread, 1).await?;
            if latest
                .first()
                .is_some_and(|turn| turn.status == "inProgress")
            {
                return Ok(());
            }
            self.withdraw_steps(thread_id).await?;
            let paused = DriveState::Paused {
                reason: format!("第 {step} 步交出后一直没有开始，已撤回；交回后重新判定"),
            };
            let state = paused.clone();
            self.change_locked(thread_id, |record| {
                withdraw_step(record, step);
                record.state = state;
            })
            .await?;
            return self.notify(thread_id, &record.title, &paused).await;
        }
        let latest = self.inner.host.session_turns(thread, 1).await?;
        if let Some(turn) = latest.first() {
            if turn.status == "inProgress" {
                // its end schedules the judgment again
                return Ok(());
            }
            // Only the turn of the last step counts against it.
            if record.steps > 0
                && record.started == Some(record.steps)
                && turn.status != "completed"
            {
                let reason = format!("第 {} 步没有正常结束（{}）", record.steps, turn.status);
                return self.settle(&record, DriveState::Paused { reason }).await;
            }
        }
        let audit = match self.inner.judge.audit(&record.project_home).await {
            Ok(audit) => audit,
            Err(err) => {
                let reason = format!("无法判定：{err:#}");
                return self
                    .settle(&record, DriveState::NeedsDecision { reason })
                    .await;
            }
        };
        let digest = (audit.project_sha256.clone(), audit.anchor_sha256.clone());
        let unchanged = match &record.digest {
            Some(previous) if *previous == digest && record.steps > 0 => record.unchanged + 1,
            Some(_) | None => 0,
        };
        let findings = || {
            if audit.findings.is_empty() {
                audit.summary()
            } else {
                audit
                    .findings
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("；")
            }
        };
        let anchor = Anchor::read(&record.project_home).unwrap_or_default();
        let next = if audit.deliverable() {
            Err(DriveState::Deliverable {
                summary: audit.summary(),
            })
        } else if audit.status == "drift" {
            Err(DriveState::NeedsDecision {
                reason: format!("偏移：{}", findings()),
            })
        } else if unchanged >= UNCHANGED_LIMIT {
            Err(DriveState::NeedsDecision {
                reason: format!(
                    "连续 {unchanged} 步 Project 和锚定记录都没有变化：worker 没有回填阶段"
                ),
            })
        } else if record.steps >= record.max_steps {
            Err(DriveState::Paused {
                reason: format!("已推进 {} 步，达到上限", record.steps),
            })
        } else if audit.status == "invalid" && audit.inconsistent.is_empty() {
            Err(DriveState::NeedsDecision {
                reason: format!("判定无效：{}", findings()),
            })
        } else {
            match focus(&anchor, &audit) {
                Some(focus) => Ok(focus),
                None => Err(DriveState::NeedsDecision {
                    reason: format!("没有可推进的验收项：{}", audit.summary()),
                }),
            }
        };
        let focus = match next {
            Ok(focus) => focus,
            Err(state) => {
                let digest = Some(digest);
                let told = state.clone();
                self.change_locked(thread_id, |record| {
                    record.state = state;
                    record.digest = digest;
                    record.unchanged = unchanged;
                })
                .await?;
                return self.notify(thread_id, &record.title, &told).await;
            }
        };
        let n = record.steps + 1;
        let prompt = compose(n, &record.title, &anchor, &audit, &focus, &record.notes);
        let focus_id = match &focus {
            Focus::Inconsistent(id) | Focus::Open(id) => id.clone(),
        };
        let step = Step {
            n,
            at: now_iso(),
            audit: audit.summary(),
            focus: focus_id,
            prompt: prompt.clone(),
        };
        self.change_locked(thread_id, |record| {
            record.steps = n;
            record.inflight = Some(n);
            record.digest = Some(digest);
            record.unchanged = unchanged;
            record.notes.clear();
            record.history.push(step);
            if record.history.len() > HISTORY {
                record.history.remove(0);
            }
        })
        .await?;
        drop(lock);
        // The session is idle: start its turn now, under the drive's own sandbox. The persistent
        // queue would hold the step after an interrupted turn, and a cold session resumes with
        // the config's defaults rather than the settings it had.
        let delivered = self.inner.host.start_step(thread, prompt).await;
        // Look again once the stall time has passed, in case the step never starts.
        self.inner
            .effects
            .enqueue_after(KIND_STEP, thread_id, &json!({}), self.inner.stall)
            .await?;
        if let Err(err) = delivered {
            let paused = DriveState::Paused {
                reason: format!("第 {n} 步没能交出：{err:#}"),
            };
            let state = paused.clone();
            let changed = self
                .change(thread_id, |record| {
                    let ours = record.inflight == Some(n);
                    if ours {
                        record.inflight = None;
                        record.state = state;
                    }
                    ours
                })
                .await?;
            if changed == Some(true) {
                self.notify(thread_id, &record.title, &paused).await?;
            }
        }
        Ok(())
    }

    /// Ends a judgment with a state and tells the Router, keeping the lock the caller holds.
    async fn settle(&self, record: &DriveRecord, state: DriveState) -> anyhow::Result<()> {
        let told = state.clone();
        self.change_locked(&record.thread_id, |record| record.state = state)
            .await?;
        self.notify(&record.thread_id, &record.title, &told).await
    }

    /// Tells the Router when a drive stops by itself (deliverable, needs a decision, paused), as a
    /// notice its next turn reads; a pause the person asked for is not told back.
    async fn notify(&self, thread_id: &str, title: &str, state: &DriveState) -> anyhow::Result<()> {
        let summary = match state {
            DriveState::Driving => return Ok(()),
            DriveState::Deliverable { summary } => {
                format!("会话 {thread_id}（{title}）可交付，等你验收：{summary}")
            }
            DriveState::NeedsDecision { reason } => {
                format!("会话 {thread_id}（{title}）的推进需要你决定：{reason}")
            }
            DriveState::Paused { reason } => {
                format!("会话 {thread_id}（{title}）的推进已暂停：{reason}")
            }
        };
        self.inner
            .effects
            .enqueue_now(
                NOTICE,
                &format!("{KEY_PREFIX}{thread_id}"),
                &json!({ "value": { "threadId": thread_id, "drive": state }, "summary": summary }),
            )
            .await
    }
}
