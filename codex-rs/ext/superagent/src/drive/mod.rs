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
mod basis;
mod context;
mod cooldown;
mod events;
mod feedback;
mod lifecycle;
mod load;
mod look;
mod notices;
mod policy;
mod pool;
mod progress;
mod report;
mod retry;
mod router_account;
mod router_rescue;
mod say;
mod standing;
mod step_account;
mod sweep;
mod switch;
mod turns;
mod wait;

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

pub use accounts::AccountInfo;
pub use accounts::AccountSource;
pub use accounts::Activity;
pub use accounts::Headroom;
pub use accounts::Plan;
pub use accounts::StatusHelper;
pub use accounts::Thresholds;
pub use accounts::Trouble;
pub use accounts::choose;
pub use accounts::choose_above;
pub use accounts::headroom;
pub use accounts::parse_account;
pub use accounts::parse_pool;
pub use accounts::pick;
pub use accounts::plan;
pub use anchor::Goal;
pub use audit::Anchor;
pub use audit::Audit;
pub use audit::Judge;
pub use audit::Oev2Judge;
pub use audit::Workspace;
pub use audit::workspace;
pub use context::DriveContext;
pub use cooldown::COOLDOWN_KEY;
pub use cooldown::Cooldown;
pub use cooldown::EndedCooldown;
pub use events::DriveStatus;
pub use feedback::Feedback;
pub use lifecycle::DriveLifecycle;
pub use look::DONE_WHILE_PAUSED;
pub use notices::KIND_NOTICE;
pub use policy::AccountPolicy;
pub use policy::DriveOptions;
pub use policy::Routers;
pub use pool::AccountMove;
pub use retry::KIND_RETRY;
pub use retry::RETRY_AFTER;
pub use retry::RETRYING;
pub use router_account::KIND_ROUTER;
pub use router_account::KIND_WATCH;
pub use router_rescue::KIND_RESCUE;
pub use router_rescue::error_trouble;
pub use router_rescue::rescue_due;
pub use say::Say;
pub use standing::STANDING_KEY;
pub use standing::StandingRule;
pub use standing::StandingScope;
pub use sweep::KIND_SWEEP;
pub use sweep::SWEEP_EVERY;
pub use wait::WAITING;
pub use wait::Wait;
pub use wait::WaitFor;
pub use wait::WaitOn;

use crate::cards::truncate_chars;
use crate::core::now_iso;
use crate::effects::EffectError;
use crate::effects::Effects;
use crate::effects::HandlerOptions;
use crate::host::StepStart;
use crate::host::SuperagentHost;
use crate::host::ThreadSpec;
use crate::store::Store;

pub const KEY_PREFIX: &str = "drive:";
pub const KIND_STEP: &str = "drive-step";
/// How the steps of earlier versions started; such a step may still wait in a session's queue.
pub const LEGACY_STEP_LABEL: &str = "[推进 ";
pub const DEFAULT_MAX_STEPS: u32 = 20;
/// How long a handed-over step may wait for its turn to start.
pub const STALL: std::time::Duration = std::time::Duration::from_secs(120);
/// Judgments in a row that found the Project and the anchor unchanged before the drive stops.
pub const UNCHANGED_LIMIT: u32 = 2;
const HISTORY: usize = 20;
const NOTES: usize = 10;
/// How many earlier conclusions a record keeps besides the latest.
const EARLIER: usize = 4;
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
    /// The session whose conclusion `builds_on` quotes, `title（thread）`, when it is another one.
    #[serde(default)]
    pub from: Option<String>,
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
    /// The conclusions before it, oldest first, at most `EARLIER`.
    #[serde(default)]
    pub earlier: Vec<String>,
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
    /// The step that went again by itself after its turn broke off on the network; it goes
    /// again only once (`retry.rs`).
    #[serde(default)]
    pub retried: Option<u32>,
    /// The Router that started the drive; its notices go there. `None` in records of earlier
    /// versions: the first registered Router gets them.
    #[serde(default)]
    pub router: Option<String>,
    /// When the Router last did something with the session (said the next thing, paused it,
    /// handed it back): a notice written before that is not delivered (`notices.rs`).
    #[serde(default)]
    pub acted: Option<String>,
    /// Why the Router's last message to the session was refused, until one goes out.
    #[serde(default)]
    pub refused: Option<String>,
    /// What the session, parked on purpose, waits for (`wait.rs`).
    #[serde(default)]
    pub wait: Option<Wait>,
    /// What the engine's round noticed about the session (`sweep.rs`), cleared when the state
    /// changes.
    #[serde(default)]
    pub marks: Vec<String>,
    /// When the state last changed, RFC 3339.
    #[serde(default)]
    pub changed: Option<String>,
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
            DriveState::Paused { reason } if self.wait.is_some() => reason.clone(),
            DriveState::Paused { reason } => format!("已暂停：{reason}"),
            DriveState::Deliverable { summary } => format!("可交付，等你验收：{summary}"),
            DriveState::NeedsDecision { reason } => format!("需要你决定：{reason}"),
        };
        let account = self
            .account
            .as_deref()
            .map(|account| format!(" | 账号 {account}"))
            .unwrap_or_default();
        let refused = self
            .refused
            .as_deref()
            .filter(|_| self.state != DriveState::Driving)
            .map(|reason| {
                format!(
                    " | 你上一句被退回（{}），这一路还在等你",
                    truncate_chars(reason, 60)
                )
            })
            .unwrap_or_default();
        let marks = if self.marks.is_empty() {
            String::new()
        } else {
            format!(" | 注意：{}", self.marks.join("；"))
        };
        format!(
            "{} | {} | 第 {}/{} 步 | {state}{account}{refused}{marks}",
            self.thread_id,
            truncate_chars(&self.title, 40),
            self.steps,
            self.max_steps
        )
    }

    /// Whether the session's turns are looked at when they end: a step is out; the worker
    /// reported and the person may still type into the session; or the drive paused, and the
    /// conclusion of the step that was running then is kept.
    fn watched(&self) -> bool {
        matches!(
            self.state,
            DriveState::Driving | DriveState::Reported { .. } | DriveState::Paused { .. }
        )
    }

    /// Makes `report` the latest conclusion; the one it replaces joins the earlier ones.
    fn take_report(&mut self, report: Report) {
        let turn = report.turn.clone();
        if let Some(old) = self.report.replace(report)
            && old.turn != turn
        {
            push_limited(&mut self.earlier, old.text, EARLIER);
            // A refused answer was to the conclusion this one replaces.
            self.refused = None;
        }
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
    /// The account the session runs its steps under, as the Router named it; without one the
    /// session keeps its account (a new session takes the Router's).
    pub account: Option<String>,
    /// The Router that starts the drive; its notices go there.
    pub router: Option<String>,
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
    /// The account pool, for moving a thread whose account runs low or cannot carry a turn.
    accounts: Option<Arc<dyn AccountSource>>,
    /// How long an account cools down after the model was at capacity on it.
    capacity_cooldown: std::time::Duration,
    /// How long a handed-over step may wait for its turn before the drive pauses.
    stall: std::time::Duration,
    /// Serializes changes to the records, so a turn's end and a tool call never interleave.
    lock: tokio::sync::Mutex<()>,
    policy: AccountPolicy,
    routers: Routers,
    /// What the account checks keep between calls (`pool.rs`).
    watch: std::sync::Mutex<pool::Watch>,
    /// One read of the pool at a time, so checks that come together share it.
    refresh: tokio::sync::Semaphore,
    /// What the turn hooks said about each driven session's latest turn (`turns.rs`).
    seen: std::sync::Mutex<HashMap<String, turns::TurnSeen>>,
    /// How long a step that broke off on the network waits before it goes again (`retry.rs`).
    retry_after: std::time::Duration,
    /// One delivery of notices at a time (`notices.rs`).
    notices: tokio::sync::Mutex<()>,
    /// Where the drives' events go (`events.rs`); `None` writes none.
    events: Option<PathBuf>,
}

impl DriveService {
    /// Builds the service with the default account policy and no Routers to watch, and
    /// registers its handlers; call before the effects start.
    pub async fn install(
        store: Store,
        effects: Effects,
        host: Arc<dyn SuperagentHost>,
        judge: Arc<dyn Judge>,
        accounts: Option<Arc<dyn AccountSource>>,
        stall: std::time::Duration,
    ) -> anyhow::Result<Self> {
        let options = DriveOptions {
            accounts,
            stall,
            policy: AccountPolicy::default(),
            routers: Arc::new(Vec::new),
            retry_after: retry::RETRY_AFTER,
            events: None,
        };
        Self::install_with(store, effects, host, judge, options).await
    }

    /// Builds the service and registers its handlers; call before the effects start.
    pub async fn install_with(
        store: Store,
        effects: Effects,
        host: Arc<dyn SuperagentHost>,
        judge: Arc<dyn Judge>,
        options: DriveOptions,
    ) -> anyhow::Result<Self> {
        let DriveOptions {
            accounts,
            stall,
            policy,
            routers,
            retry_after,
            events,
        } = options;
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
                policy,
                routers,
                watch: std::sync::Mutex::new(pool::Watch::default()),
                refresh: tokio::sync::Semaphore::new(1),
                seen: std::sync::Mutex::new(HashMap::new()),
                retry_after,
                notices: tokio::sync::Mutex::new(()),
                events,
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
        router_account::install_router_handlers(&service, &effects).await?;
        retry::install_retry_handler(&service, &effects).await?;
        notices::install_notice_handler(&service, &effects).await?;
        sweep::install_sweep_handler(&service, &effects).await?;
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
        let changed = self
            .inner
            .store
            .atomic(async |tx| {
                let Some(mut record) = tx.kv_get::<DriveRecord>(&key).await? else {
                    return Ok(None);
                };
                let before = (record.state.clone(), record.steps, record.account.clone());
                let result = change(&mut record);
                let now = now_iso();
                if record.state != before.0 {
                    record.changed = Some(now.clone());
                    record.marks.clear();
                }
                record.updated = now;
                tx.kv_set(&key, &record).await?;
                let moved = record.state != before.0
                    || record.steps != before.1
                    || record.account != before.2;
                Ok(Some((result, moved.then_some(record))))
            })
            .await?;
        Ok(changed.map(|(result, moved)| {
            if let Some(record) = moved {
                self.log_event(&record, /*event*/ None);
            }
            result
        }))
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
        let profile = match asked_by.map(ThreadId::from_string) {
            Some(Ok(thread)) => self.inner.host.thread_profile(thread).await?,
            Some(Err(_)) | None => None,
        };
        self.open_session_under(folder, profile).await
    }

    async fn open_session_under(
        &self,
        folder: &Path,
        profile: Option<String>,
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
            account,
            router,
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
        // A drive started on a named account runs from its first step there (`pool.rs`).
        let account = match account.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
            Some(wanted) => Some(
                self.bind_start_account(thread_id, thread, wanted, &now)
                    .await?,
            ),
            None => None,
        };
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
            earlier: Vec::new(),
            account,
            unchanged: 0,
            digest: None,
            notes: Vec::new(),
            inflight: Some(1),
            started: None,
            retried: None,
            router,
            acted: Some(now.clone()),
            refused: None,
            wait: None,
            marks: Vec::new(),
            changed: Some(now.clone()),
            history: vec![Step {
                n: 1,
                at: now.clone(),
                audit: String::new(),
                text: first.clone(),
                builds_on: String::new(),
                serves: truncate_chars(request.trim(), 200),
                steered: false,
                from: None,
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
        self.log_event(&record, Some("started"));
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
            record.acted = Some(now_iso());
            record.refused = None;
            record.wait = None;
            record.clone()
        })
        .await
    }

    /// Hands the session back: the Router answers the last conclusion with `say`. A stop that
    /// waits for the person stays until the person's words go out with `say`.
    pub async fn resume(&self, thread_id: &str) -> anyhow::Result<Option<DriveRecord>> {
        self.change(thread_id, |record| {
            // Handing back is not progress: the count of steps without any runs on.
            match record.state {
                DriveState::Driving | DriveState::NeedsDecision { .. } => {}
                DriveState::Reported { .. }
                | DriveState::Paused { .. }
                | DriveState::Deliverable { .. } => {
                    record.state = DriveState::Reported {
                        summary: "推进已交回，等你回它下一句".to_owned(),
                    };
                }
            }
            record.acted = Some(now_iso());
            record.refused = None;
            record.wait = None;
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
            record.acted = Some(now_iso());
            record.wait = None;
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
            self.log_event(record, Some("released"));
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
                record.acted = Some(now_iso());
                record.clone()
            })
            .await?;
        if record.is_some() {
            self.inner.host.interrupt(thread).await?;
        }
        Ok(record)
    }

    /// What the person typed into a driven session; the Router sees it with the next conclusion.
    pub async fn person_input(&self, thread_id: &str, text: &str) -> anyhow::Result<()> {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return Ok(());
        }
        self.change(thread_id, |record| {
            // the drive's own step arrives as the session's input too
            if record
                .history
                .last()
                .is_some_and(|step| step.text.trim() == text)
            {
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
        // The step goes out on an account with headroom, moved before it when need be.
        self.check_step_account(thread_id, thread, n).await;
        // Under the drive's own sandbox: a cold session resumes with the config's defaults
        // rather than the settings it had.
        let delivered = self.inner.host.start_step(thread, text).await;
        // Look again once the stall time has passed, in case the step never starts.
        self.inner
            .effects
            .enqueue_after(KIND_STEP, thread_id, &json!({}), self.inner.stall)
            .await?;
        match delivered {
            Ok(StepStart::Started { .. }) => {}
            // A turn ran already (the person's, or a step still running when the drive paused):
            // the step's words joined it, no turn of its own starts, and that turn's end answers
            // the step.
            Ok(StepStart::Steered { turn }) => {
                self.change(thread_id, |record| {
                    if record.inflight == Some(n) {
                        record.inflight = None;
                        record.started = Some(n);
                    }
                })
                .await?;
                if let Some(turn) = turn {
                    self.adopt_turn(thread_id, &turn);
                }
            }
            Err(err) => {
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
        }
        Ok(())
    }
}

/// Keeps the newest `limit` items.
fn push_limited(items: &mut Vec<String>, item: String, limit: usize) {
    items.push(item);
    if items.len() > limit {
        items.remove(0);
    }
}
