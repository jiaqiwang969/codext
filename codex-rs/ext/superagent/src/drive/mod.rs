//! The drive loop (command-center architecture §5–§6; daemon integration P2): the Router keeps
//! a session moving on the person's behalf. The engine never writes a step itself. At the end of
//! each of the session's turns it reads the worker's conclusion (and, for an anchored goal, runs
//! the engineering ontology's `route-audit`), records it and hands it to the Router, which tells
//! the person or answers the worker with `say` (`say.rs`), quoting what it builds on.
//!
//! One record per driven session lives in the ledger's `kv` table (`drive:<thread id>`): the
//! person's words, the worker's last conclusion and every step with its basis, so neither a
//! restart nor the Router's compacted memory loses them. A step is recorded as in flight before
//! it is handed over and cleared when its turn starts; a recovered step that never started is not
//! replayed.

mod accounts;
mod anchor;
mod audit;
mod context;
mod feedback;
mod lifecycle;
mod report;
mod say;
mod switch;

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

pub use accounts::AccountInfo;
pub use accounts::AccountSource;
pub use accounts::Cooldown;
pub use accounts::StatusHelper;
pub use accounts::Trouble;
pub use accounts::choose;
pub use accounts::parse_pool;
pub use anchor::Goal;
pub use audit::Anchor;
pub use audit::Audit;
pub use audit::Judge;
pub use audit::Oev2Judge;
pub use audit::Workspace;
pub use audit::workspace;
pub use context::DriveContext;
pub use feedback::Feedback;
pub use lifecycle::DriveLifecycle;
pub use say::Say;

use crate::cards::truncate_chars;
use crate::core::now_iso;
use crate::effects::EffectError;
use crate::effects::Effects;
use crate::effects::HandlerOptions;
use crate::host::SuperagentHost;
use crate::host::ThreadSpec;
use crate::store::Store;

pub const KEY_PREFIX: &str = "drive:";
pub const KIND_STEP: &str = "drive-step";
/// How the steps of earlier versions started; such a step may still wait in a session's queue.
pub const LEGACY_STEP_LABEL: &str = "[推进 ";
/// The effect kind that delivers a notice to the Router (`pm.rs`).
const NOTICE: &str = "notice";
pub const DEFAULT_MAX_STEPS: u32 = 20;
/// How long a handed-over step may wait for its turn to start.
pub const STALL: std::time::Duration = std::time::Duration::from_secs(120);
/// Judgments in a row that found the Project and the anchor unchanged before the drive stops.
pub const UNCHANGED_LIMIT: u32 = 2;
const HISTORY: usize = 20;
const NOTES: usize = 10;
/// The most of a worker's conclusion the record keeps.
const REPORT_CHARS: usize = 4_000;

pub fn drive_key(thread_id: &str) -> String {
    format!("{KEY_PREFIX}{thread_id}")
}

/// Takes back a step that never reached its session: it no longer counts.
fn withdraw_step(record: &mut DriveRecord, step: u32) {
    record.inflight = None;
    if record.steps == step {
        record.steps -= 1;
        record.history.pop();
    }
}

/// How a driven goal is judged.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DriveMode {
    /// One request in the person's words, done in a turn or a few: no anchor, no audit; the
    /// person accepts the result.
    Simple,
    /// A goal anchored in the project's `.engineering`; `route-audit` judges every turn's end.
    #[default]
    Anchored,
}

/// Who moves a driven session now.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DriveState {
    /// A step is with the worker.
    Driving,
    /// The worker ended its turn; the Router tells the person or answers the worker.
    Reported { summary: String },
    /// The person took the session over, or the engine paused it; nothing is sent until the
    /// Router speaks again.
    Paused { reason: String },
    /// Every acceptance item is verified; the person accepts the delivery.
    Deliverable { summary: String },
    /// The engine stopped and needs the person: drift, no progress, or an audit it cannot run.
    NeedsDecision { reason: String },
}

/// One step sent to the session: what it said and what it follows from.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Step {
    pub n: u32,
    pub at: String,
    /// The judgment the step answered (anchored goals).
    #[serde(default)]
    pub audit: String,
    /// What the session was told.
    #[serde(alias = "prompt")]
    pub text: String,
    /// The words (of the last conclusion, the request or a correction) the step follows from.
    #[serde(default)]
    pub builds_on: String,
    /// The words of the request (or a correction, or an acceptance item) the step serves.
    #[serde(default)]
    pub serves: String,
    /// Said into the running turn of step `n` (the person corrected course), not a turn of its own.
    #[serde(default)]
    pub steered: bool,
}

/// The worker's conclusion at the end of a turn.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Report {
    pub turn: String,
    pub at: String,
    pub text: String,
    /// The judgment of the turn (anchored goals).
    #[serde(default)]
    pub audit: Option<String>,
}

/// A driven session.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DriveRecord {
    pub thread_id: String,
    pub title: String,
    #[serde(default)]
    pub mode: DriveMode,
    /// Anchored: the goal's Project workspace, `<project>/.engineering`. Simple: the folder the
    /// session works in.
    pub project_home: PathBuf,
    pub state: DriveState,
    pub steps: u32,
    pub max_steps: u32,
    /// The person's request, word for word.
    #[serde(default)]
    pub request: String,
    /// What the person later told the Router about this goal, word for word, oldest first.
    #[serde(default)]
    pub corrections: Vec<String>,
    /// The worker's last conclusion.
    #[serde(default)]
    pub report: Option<Report>,
    /// The account the drive last moved the session to.
    #[serde(default)]
    pub account: Option<String>,
    /// Judgments in a row that found the same Project and anchor digests.
    pub unchanged: u32,
    /// (Project, anchor) digests of the last judgment.
    pub digest: Option<(String, String)>,
    /// What the person typed into the session itself since the last step.
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
            DriveState::Driving => "worker 正在做".to_owned(),
            DriveState::Reported { summary } => format!("交了结论：{summary}"),
            DriveState::Paused { reason } => format!("已暂停：{reason}"),
            DriveState::Deliverable { summary } => format!("可交付，等你验收：{summary}"),
            DriveState::NeedsDecision { reason } => format!("需要你决定：{reason}"),
        };
        let account = self
            .account
            .as_deref()
            .map(|account| format!(" | 账号 {account}"))
            .unwrap_or_default();
        format!(
            "{} | {} | 第 {}/{} 步 | {state}{account}",
            self.thread_id,
            truncate_chars(&self.title, 40),
            self.steps,
            self.max_steps
        )
    }

    /// Whether the session's turns are judged when they end (a step is out, or the worker
    /// reported and the person may still type into the session).
    fn watched(&self) -> bool {
        matches!(
            self.state,
            DriveState::Driving | DriveState::Reported { .. }
        )
    }
}

/// What a drive starts from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveStart {
    /// Anchored: the goal's Project workspace, `<project>/.engineering`. Simple: the folder.
    pub project_home: PathBuf,
    pub title: String,
    pub mode: DriveMode,
    /// The most steps to send before pausing; the default when absent.
    pub max_steps: Option<u32>,
    /// The person's request, word for word.
    pub request: String,
    /// Anchored: the goal the person confirmed, anchored when the project has no anchor yet; an
    /// existing anchor is used as it is.
    pub goal: Option<Goal>,
    /// The first message to the session, in the Router's words.
    pub first: String,
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
    /// The account pool, for moving a session whose account cannot carry a step.
    accounts: Option<Arc<dyn AccountSource>>,
    /// How long an account cools down after the model was at capacity on it.
    capacity_cooldown: std::time::Duration,
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
        accounts: Option<Arc<dyn AccountSource>>,
        stall: std::time::Duration,
    ) -> anyhow::Result<Self> {
        let service = Self {
            inner: Arc::new(Inner {
                stall,
                accounts,
                capacity_cooldown: accounts::CAPACITY_COOLDOWN,
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

    /// Whether a step is with the session now (paused and stopped sessions are the person's).
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

    /// Opens a new session in `folder` (created when missing) under the account of the thread
    /// that asks, so a one-sentence request needs nothing but a folder.
    pub async fn open_session(
        &self,
        folder: &Path,
        asked_by: Option<&str>,
    ) -> anyhow::Result<String> {
        anyhow::ensure!(
            folder.is_absolute(),
            "文件夹要写绝对路径：{}",
            folder.display()
        );
        anyhow::ensure!(
            !folder.is_file(),
            "{} 是一个文件，不是文件夹",
            folder.display()
        );
        std::fs::create_dir_all(folder)?;
        let profile = match asked_by.map(ThreadId::from_string) {
            Some(Ok(thread)) => self.inner.host.thread_profile(thread).await?,
            Some(Err(_)) | None => None,
        };
        let thread = self
            .inner
            .host
            .start_thread(ThreadSpec {
                cwd: folder.to_owned(),
                profile,
                ..ThreadSpec::default()
            })
            .await?;
        Ok(thread.to_string())
    }

    /// Takes a session under drive and sends it the first message at once.
    pub async fn start(&self, thread_id: &str, start: DriveStart) -> anyhow::Result<DriveRecord> {
        let thread = ThreadId::from_string(thread_id)
            .map_err(|err| anyhow::anyhow!("会话 id {thread_id} 无效：{err}"))?;
        let DriveStart {
            project_home,
            title,
            mode,
            max_steps,
            request,
            goal,
            first,
        } = start;
        anyhow::ensure!(!request.trim().is_empty(), "需要用户的原话（request）");
        let first = say::spoken(&first)?;
        match mode {
            DriveMode::Anchored => {
                // A project without an anchor gets one from the goal the person confirmed.
                if let Some(goal) = &goal
                    && workspace(&project_home).is_err()
                {
                    anchor::create(&project_home, goal)?;
                }
                workspace(&project_home)?;
            }
            DriveMode::Simple => anyhow::ensure!(
                project_home.is_dir(),
                "{} 不是已有的文件夹",
                project_home.display()
            ),
        }
        let now = now_iso();
        let record = DriveRecord {
            thread_id: thread_id.to_owned(),
            title,
            mode,
            project_home,
            state: DriveState::Driving,
            steps: 1,
            max_steps: max_steps.filter(|n| *n > 0).unwrap_or(DEFAULT_MAX_STEPS),
            request: request.trim().to_owned(),
            corrections: Vec::new(),
            report: None,
            account: None,
            unchanged: 0,
            digest: None,
            notes: Vec::new(),
            inflight: Some(1),
            started: None,
            history: vec![Step {
                n: 1,
                at: now.clone(),
                audit: String::new(),
                text: first.clone(),
                builds_on: String::new(),
                serves: truncate_chars(request.trim(), 200),
                steered: false,
            }],
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
                            "会话 {thread_id} 已经在推进记录里（{}）；继续用 say，或 stop 释放",
                            existing.line()
                        );
                    }
                    tx.kv_set(&key, &record).await
                })
                .await?;
        }
        self.hand_over(thread_id, thread, &record.title, 1, first)
            .await?;
        Ok(self.record(thread_id).await?.unwrap_or(record))
    }

    /// The person takes the session over: nothing is sent until the Router speaks again.
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

    /// Hands the session back: the Router answers the last conclusion with `say`.
    pub async fn resume(&self, thread_id: &str) -> anyhow::Result<Option<DriveRecord>> {
        self.change(thread_id, |record| {
            if record.state != DriveState::Driving {
                record.state = DriveState::Reported {
                    summary: "推进已交回，等你回它下一句".to_owned(),
                };
                record.unchanged = 0;
            }
            record.clone()
        })
        .await
    }

    /// The person's acceptance feedback on an anchored goal: written into the anchor
    /// (`feedback.rs`) and kept as a correction; the Router then answers the worker with `say`.
    pub async fn feedback(
        &self,
        thread_id: &str,
        feedback: Feedback,
    ) -> anyhow::Result<Option<DriveRecord>> {
        let Some(record) = self.record(thread_id).await? else {
            return Ok(None);
        };
        anyhow::ensure!(
            record.mode == DriveMode::Anchored,
            "这个目标没有锚定记录；用户的话直接用 say 带给 worker（correction 填原话）"
        );
        self.ensure_heard(&feedback.text).await?;
        let request = feedback::record(&record.project_home, &feedback)?;
        let text = feedback.text.trim().to_owned();
        self.change(thread_id, |record| {
            push_limited(&mut record.corrections, text, NOTES);
            if record.state != DriveState::Driving {
                record.state = DriveState::Reported {
                    summary: format!("用户的验收反馈已记入锚定记录 {request}，等你回 worker"),
                };
            }
            record.unchanged = 0;
            record.clone()
        })
        .await
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
        if let Some(record) = &record {
            self.withdraw_steps(record).await?;
        }
        Ok(record)
    }

    /// Removes the drive's steps still waiting in the session's persistent queue.
    async fn withdraw_steps(&self, record: &DriveRecord) -> anyhow::Result<usize> {
        let thread = ThreadId::from_string(&record.thread_id)?;
        let mut withdrawn = 0;
        for (item, text) in self.inner.host.queued(thread).await? {
            let ours = text.trim_start().starts_with(LEGACY_STEP_LABEL)
                || record.history.iter().any(|step| step.text == text);
            if ours && self.inner.host.withdraw_queued(thread, item).await? {
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

    /// A turn ended: its conclusion is read next.
    pub async fn turn_stopped(&self, thread_id: &str) -> anyhow::Result<()> {
        if self
            .record(thread_id)
            .await?
            .is_some_and(|record| record.watched())
        {
            self.schedule(thread_id).await?;
        }
        Ok(())
    }

    /// The Router stops the running step (the person found it going wrong): the turn is
    /// interrupted and the drive pauses until the Router hands it back and says the next thing.
    pub async fn interrupt(&self, thread_id: &str) -> anyhow::Result<Option<DriveRecord>> {
        let thread = ThreadId::from_string(thread_id)
            .map_err(|err| anyhow::anyhow!("会话 id {thread_id} 无效：{err}"))?;
        let record = self
            .change(thread_id, |record| {
                if record.state == DriveState::Driving {
                    record.state = DriveState::Paused {
                        reason: format!("第 {} 步被打断；交回后说下一句", record.steps),
                    };
                }
                record.clone()
            })
            .await?;
        if record.is_some() {
            self.inner.host.interrupt(thread).await?;
        }
        Ok(record)
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

    /// What the person typed into a driven session; the Router sees it with the next conclusion.
    pub async fn person_input(&self, thread_id: &str, text: &str) -> anyhow::Result<()> {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return Ok(());
        }
        self.change(thread_id, |record| {
            // the drive's own step arrives as the session's input too
            if record.history.last().is_some_and(|step| step.text.trim() == text) {
                return;
            }
            push_limited(&mut record.notes, text, NOTES);
        })
        .await
        .map(drop)
    }

    /// After a restart: a step handed over but never started is not replayed. Its queued text
    /// is withdrawn and the drive pauses; every other step out is looked at again.
    pub async fn recover(&self) -> anyhow::Result<()> {
        for record in self.records().await? {
            if record.state != DriveState::Driving {
                continue;
            }
            match record.inflight {
                Some(step) => {
                    self.withdraw_steps(&record).await?;
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
                    self.notify(&record, &DriveState::Paused { reason }).await?;
                }
                None => self.schedule(&record.thread_id).await?,
            }
        }
        Ok(())
    }

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
        let latest = self.inner.host.session_turns(thread, 1).await?;
        let Some(turn) = latest.first() else {
            return Ok(());
        };
        if turn.status == "inProgress" {
            // its end looks again
            return Ok(());
        }
        match &record.state {
            DriveState::Driving => {
                if record.started != Some(record.steps) {
                    return Ok(());
                }
                if turn.status != "completed" {
                    // The account could not carry the step: move the session and send it again.
                    if let Some(trouble) = accounts::trouble(turn)
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
                    let reason = match &turn.error {
                        Some(error) => format!(
                            "第 {} 步没有正常结束（{}：{}）",
                            record.steps,
                            turn.status,
                            truncate_chars(error, 200)
                        ),
                        None => format!("第 {} 步没有正常结束（{}）", record.steps, turn.status),
                    };
                    return self.settle(&record, DriveState::Paused { reason }, None).await;
                }
            }
            // The person typed into the session after the conclusion: a newer one replaces it.
            DriveState::Reported { .. } => {
                if turn.status != "completed"
                    || record.report.as_ref().is_some_and(|report| report.turn == turn.id)
                {
                    return Ok(());
                }
            }
            DriveState::Paused { .. }
            | DriveState::Deliverable { .. }
            | DriveState::NeedsDecision { .. } => return Ok(()),
        }
        let conclusion = report::conclusion(turn);
        let mut report = Report {
            turn: turn.id.clone(),
            at: now_iso(),
            text: truncate_chars(&conclusion, REPORT_CHARS),
            audit: None,
        };
        let (state, digest, unchanged) = match record.mode {
            DriveMode::Simple => (report::reported(&record, &report.text), None, 0),
            DriveMode::Anchored => {
                let audit = match self.inner.judge.audit(&record.project_home).await {
                    Ok(audit) => audit,
                    Err(err) => {
                        let reason = format!("无法判定：{err:#}");
                        return self
                            .settle(&record, DriveState::NeedsDecision { reason }, Some(report))
                            .await;
                    }
                };
                report.audit = Some(audit.summary());
                let digest = (audit.project_sha256.clone(), audit.anchor_sha256.clone());
                let unchanged = match &record.digest {
                    Some(previous) if *previous == digest => record.unchanged + 1,
                    Some(_) | None => 0,
                };
                (
                    report::judged(&record, &report.text, &audit, unchanged),
                    Some(digest),
                    unchanged,
                )
            }
        };
        let state = match state {
            DriveState::Reported { .. } if record.steps >= record.max_steps => DriveState::Paused {
                reason: format!("已推进 {} 步，达到上限；放宽要问用户", record.steps),
            },
            state => state,
        };
        let told = state.clone();
        let kept = report.clone();
        self.change_locked(thread_id, |record| {
            record.state = state;
            record.report = Some(report);
            if let Some(digest) = digest {
                record.digest = Some(digest);
                record.unchanged = unchanged;
            }
        })
        .await?;
        let record = DriveRecord {
            report: Some(kept),
            ..record
        };
        self.notify(&record, &told).await
    }

    /// A step handed over and not started: wait, unless it has waited too long; then it is
    /// withdrawn and the drive pauses.
    async fn check_stall(
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

    /// Starts step `n`'s turn on the idle session, outside the lock: the turn's start and input
    /// hooks change the record too. A failed hand-over pauses the drive and tells the Router.
    async fn hand_over(
        &self,
        thread_id: &str,
        thread: ThreadId,
        title: &str,
        n: u32,
        text: String,
    ) -> anyhow::Result<()> {
        // Under the drive's own sandbox: a cold session resumes with the config's defaults
        // rather than the settings it had.
        let delivered = self.inner.host.start_step(thread, text).await;
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
                    ours.then(|| record.clone())
                })
                .await?;
            if let Some(Some(record)) = changed {
                self.notify(&record, &paused).await?;
            } else {
                tracing::warn!("superagent drive: step {n} of {title} was not handed over");
            }
        }
        Ok(())
    }

    /// Ends a look with a state and tells the Router, keeping the lock the caller holds.
    async fn settle(
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
                record.report = Some(report);
            }
        })
        .await?;
        let record = DriveRecord {
            report: kept.or_else(|| record.report.clone()),
            ..record.clone()
        };
        self.notify(&record, &told).await
    }

    /// Tells the Router when the worker reported or the drive stopped by itself, as a notice its
    /// next turn reads; a pause the person asked for is not told back.
    async fn notify(&self, record: &DriveRecord, state: &DriveState) -> anyhow::Result<()> {
        let Some(summary) = report::notice(record, state) else {
            return Ok(());
        };
        let thread_id = &record.thread_id;
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

/// Keeps the newest `limit` items.
fn push_limited(items: &mut Vec<String>, item: String, limit: usize) {
    items.push(item);
    if items.len() > limit {
        items.remove(0);
    }
}
