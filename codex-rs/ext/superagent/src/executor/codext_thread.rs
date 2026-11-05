//! The first executor: one codext root thread per ticket (design §6, §6.7). Descends from the
//! prototype's Codex pilot (`src/executor/codex/executor.ts`): one serial lane per ticket, a session
//! record that pins the account and the thread, an `inflight` row written before every delivery and
//! never replayed on its own, and `ready` downgraded to `progress` when inputs arrived meanwhile.
//! The launch and stop rules come from `src/executor/dsh.ts`; the turn-end rules from
//! `src/executor/background.ts`.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use codex_extension_api::ExtensionFuture;
use codex_extension_api::TurnAbortInput;
use codex_extension_api::TurnErrorInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStartInput;
use codex_extension_api::TurnStopInput;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use sqlx::Row;
use tokio::sync::Semaphore;

use super::EngineForExecutor;
use super::Executor;
use super::WorkerReport;
use super::accounts;
use super::background;
use super::background::TurnEnd;
use super::slots::SlotBy;
use super::slots::WorkerSlots;
use crate::BoxFut;
use crate::core::Assignment;
use crate::core::AssignmentState;
use crate::core::ReportState;
use crate::core::Ticket;
use crate::core::TicketInput;
use crate::core::latest_assignment;
use crate::core::waits_for_person;
use crate::effects::Clock;
use crate::effects::EffectError;
use crate::effects::Effects;
use crate::effects::HandlerOptions;
use crate::effects::iso;
use crate::host::DeliveryMode;
use crate::host::GoalStatus;
use crate::host::SuperagentHost;
use crate::host::ThreadSpec;
use crate::store::Store;
use crate::store::Tx;

mod capacity;

pub const KIND_LAUNCH: &str = "launch";
pub const KIND_DELIVERY: &str = "delivery";
/// A continuation after capacity incidents (design §6.8); key = assignment id.
pub const KIND_CONTINUE: &str = "continue";
/// Back to the role model after the restore delay; key = thread id.
pub const KIND_MODEL_RESTORE: &str = "model-restore";
/// What `assignment.executor_kind` records for this executor.
pub const EXECUTOR_KIND: &str = "codext_thread";
/// The note a leftover `inflight` row earns a ticket on restore (decision #8: stop and ask).
pub const RECOVERY_NOTE: &str = "服务重启时这张工单有一次发出但没有看到结束的投递；没有自动重放。请检查 worker 线程和工作目录后再决定重试还是停止。";

/// What `restore` did at start, by ticket display name (design §8.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestoreReport {
    /// Pending assignments queued for launch again.
    pub relaunched: Vec<String>,
    /// Running assignments the engine restarted with a recovery note.
    pub restarted: Vec<String>,
    /// Running assignments with a delivery in flight: blocked, for the person to inspect.
    pub blocked: Vec<String>,
    /// Pending launches left alone because the ticket waits for a person.
    pub waiting: Vec<String>,
    /// Tickets that needed nothing: settled, already told, or without a session.
    pub untouched: usize,
}
const MAX_ASSIGNMENT_CHARS: usize = 1500;
const MAX_INPUT_CHARS: usize = 600;
/// How often a worker's background commands are looked at after its turn (design §1.4).
const BACKGROUND_POLL: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct ExecutorConfig {
    /// The repository the worktrees are created from.
    pub repo: PathBuf,
    pub worktree_root: PathBuf,
    pub base_branch: String,
    /// Whether to run `git worktree add`; off in tests.
    pub create_worktrees: bool,
    /// The account profiles workers may run under.
    pub profiles: Vec<String>,
    pub max_per_profile: usize,
    pub max_workers: usize,
    pub max_per_pm: usize,
    /// Where the ended worker threads are listed for the PreToolUse fallback hook.
    pub ended_threads_file: PathBuf,
    /// The PM thread list; a PM's line may name its project (repository, base branch, worktree
    /// root), which its tickets use instead of `repo`, `base_branch` and `worktree_root`.
    pub pm_threads_file: Option<PathBuf>,
    pub worker_role: String,
    pub model: Option<String>,
    /// Tokens one assignment may use (codext's per-thread count, cached input included); over it,
    /// the assignment is reported blocked at the end of the turn. None: no budget.
    pub token_budget: Option<i64>,
    pub background_limit: Duration,
    /// Capacity policy (design §6.8). After `capacity_threshold` capacity/limit incidents on a
    /// worker thread within `capacity_window`, the thread runs on `capacity_fallback_model`
    /// (None: policy off) at `capacity_fallback_effort`, and goes back to its role file after
    /// `capacity_restore` without incidents. A terminal capacity failure without a report may
    /// continue after `capacity_continue_delay` only while its ticket and assignment are unchanged,
    /// at most `capacity_continue_max` times per assignment, including across restarts.
    pub capacity_fallback_model: Option<String>,
    pub capacity_fallback_effort: Option<String>,
    pub capacity_threshold: usize,
    pub capacity_window: Duration,
    pub capacity_restore: Duration,
    pub capacity_continue_delay: Duration,
    pub capacity_continue_max: u32,
}

/// The default per-assignment budget. Measured on dell 11/03: finished assignments used 70K to
/// 6M (median about 550K); a looping worker at 150K per request hits this in about 100 requests.
pub const DEFAULT_TOKEN_BUDGET: i64 = 15_000_000;

impl ExecutorConfig {
    /// The A1 layout on dell under the org user's home.
    pub fn for_home(home: &Path) -> Self {
        Self {
            repo: home.join("src/codext"),
            worktree_root: home.join("wt"),
            base_branch: "superagent/integration".to_owned(),
            create_worktrees: true,
            profiles: vec!["JannLinegarA0k".to_owned(), "kalliesallerwu".to_owned()],
            max_per_profile: 3,
            max_workers: 6,
            max_per_pm: 4,
            ended_threads_file: home.join("org/pm-codext/state/ended-threads"),
            pm_threads_file: Some(home.join(".codex/superagent/pm-threads")),
            worker_role: "worker".to_owned(),
            model: None,
            token_budget: Some(DEFAULT_TOKEN_BUDGET),
            background_limit: background::DEFAULT_BACKGROUND_LIMIT,
            capacity_fallback_model: None,
            capacity_fallback_effort: Some("high".to_owned()),
            capacity_threshold: 2,
            capacity_window: Duration::from_secs(600),
            capacity_restore: Duration::from_secs(1800),
            capacity_continue_delay: Duration::from_secs(120),
            capacity_continue_max: 3,
        }
    }
}

/// The binding between a ticket and its codext thread, kept in kv under `codext-session:<ticket>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub account: String,
    pub thread_id: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Deliveries made so far; the first assignment message goes out only before any.
    #[serde(default)]
    pub turns: u32,
    /// The thread's token count when the current assignment started; the budget counts from here.
    #[serde(default)]
    pub tokens_at_start: i64,
}

pub fn session_key(ticket_id: &str) -> String {
    format!("codext-session:{ticket_id}")
}

#[derive(Clone, Debug, PartialEq)]
pub struct Inflight {
    pub thread_id: String,
    pub assignment_id: String,
    pub input_ids: Vec<String>,
    pub turn_id: Option<String>,
    pub started_at: String,
}

#[derive(Clone, Debug)]
struct TurnState {
    ticket_id: String,
    assignment_id: String,
    turn_id: String,
    capacity_failed: bool,
    reported: Option<ReportState>,
    question: bool,
}

struct Inner {
    store: Store,
    effects: Effects,
    engine: Arc<dyn EngineForExecutor>,
    host: Arc<dyn SuperagentHost>,
    config: ExecutorConfig,
    slots: WorkerSlots,
    clock: Clock,
    lanes: Mutex<HashMap<String, Arc<Semaphore>>>,
    /// The session records loaded this process, by ticket.
    sessions: Mutex<HashMap<String, SessionRecord>>,
    /// The turns running now, by thread.
    turns: Mutex<HashMap<String, TurnState>>,
    /// Threads on the fallback model, since when.
    fallbacks: Mutex<HashMap<String, Instant>>,
    paused: AtomicBool,
}

#[derive(Clone)]
pub struct CodextThreadExecutor {
    inner: Arc<Inner>,
}

impl CodextThreadExecutor {
    pub fn new(
        store: Store,
        effects: Effects,
        engine: Arc<dyn EngineForExecutor>,
        host: Arc<dyn SuperagentHost>,
        config: ExecutorConfig,
        clock: Clock,
    ) -> Self {
        let slots = WorkerSlots::new(config.max_workers, config.max_per_pm);
        Self {
            inner: Arc::new(Inner {
                store,
                effects,
                engine,
                host,
                config,
                slots,
                clock,
                lanes: Mutex::new(HashMap::new()),
                sessions: Mutex::new(HashMap::new()),
                turns: Mutex::new(HashMap::new()),
                fallbacks: Mutex::new(HashMap::new()),
                paused: AtomicBool::new(false),
            }),
        }
    }

    pub fn slots(&self) -> &WorkerSlots {
        &self.inner.slots
    }

    pub fn set_paused(&self, paused: bool) {
        self.inner.paused.store(paused, Ordering::SeqCst);
    }

    fn now(&self) -> i64 {
        (self.inner.clock)()
    }

    /// The ticket a worker thread runs, if the thread is one of ours.
    pub fn ticket_of_thread(&self, thread_id: &str) -> Option<String> {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|(_, record)| record.thread_id == thread_id)
            .map(|(ticket, _)| ticket.clone())
    }

    /// Whether a turn runs on the thread now.
    pub fn turn_running(&self, thread_id: &str) -> bool {
        self.inner
            .turns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(thread_id)
    }

    fn remember(&self, ticket_id: &str, record: &SessionRecord) {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(ticket_id.to_owned(), record.clone());
    }

    fn lane(&self, ticket_id: &str) -> Arc<Semaphore> {
        Arc::clone(
            self.inner
                .lanes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(ticket_id.to_owned())
                .or_insert_with(|| Arc::new(Semaphore::new(1))),
        )
    }

    /// Running workers per account profile, for the account choice.
    fn running_by_profile(&self) -> HashMap<String, usize> {
        let sessions = self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut counts = HashMap::new();
        for (ticket, record) in sessions.iter() {
            if self.inner.slots.holds(ticket) {
                *counts.entry(record.account.clone()).or_insert(0) += 1;
            }
        }
        counts
    }

    /// The ticket, if it is open and the assignment is still its live one.
    async fn current(
        &self,
        ticket_id: &str,
        assignment_id: &str,
    ) -> anyhow::Result<Option<Ticket>> {
        let Some(ticket) = self.inner.store.ticket(ticket_id).await? else {
            return Ok(None);
        };
        if ticket.ended.is_some() {
            return Ok(None);
        }
        let live = latest_assignment(&ticket)
            .is_some_and(|a| a.id == assignment_id && a.state != AssignmentState::Ended);
        Ok(live.then_some(ticket))
    }

    pub async fn session(&self, ticket_id: &str) -> anyhow::Result<Option<SessionRecord>> {
        self.inner.store.kv_get(&session_key(ticket_id)).await
    }

    pub async fn inflight_for(&self, thread_id: &str) -> anyhow::Result<Option<Inflight>> {
        let row = sqlx::query(
            "SELECT thread_id, assignment_id, input_ids, turn_id, started_at FROM inflight WHERE thread_id = ?",
        )
        .bind(thread_id)
        .fetch_optional(self.inner.store.pool())
        .await?;
        Ok(match row {
            Some(row) => Some(Inflight {
                thread_id: row.try_get("thread_id")?,
                assignment_id: row.try_get("assignment_id")?,
                input_ids: serde_json::from_str(&row.try_get::<String, _>("input_ids")?)?,
                turn_id: row.try_get("turn_id")?,
                started_at: row.try_get("started_at")?,
            }),
            None => None,
        })
    }

    async fn write_inflight(
        tx: &mut Tx,
        thread_id: &str,
        assignment_id: &str,
        input_ids: &[String],
        started_at: &str,
    ) -> anyhow::Result<()> {
        // Deliveries queued before a turn starts are all consumed by the turns that follow, in
        // order; the row keeps every one of them until a turn starts, so none is left unread.
        let waiting = sqlx::query(
            "SELECT assignment_id, input_ids FROM inflight WHERE thread_id = ? AND turn_id IS NULL",
        )
        .bind(thread_id)
        .fetch_optional(tx.conn())
        .await?;
        let mut merged: Vec<String> = match waiting {
            Some(row) if row.try_get::<String, _>("assignment_id")? == assignment_id => {
                serde_json::from_str(&row.try_get::<String, _>("input_ids")?)?
            }
            _ => Vec::new(),
        };
        for id in input_ids {
            if !merged.contains(id) {
                merged.push(id.clone());
            }
        }
        sqlx::query(
            "INSERT INTO inflight (thread_id, assignment_id, input_ids, turn_id, started_at) VALUES (?, ?, ?, NULL, ?)
             ON CONFLICT(thread_id) DO UPDATE SET assignment_id = excluded.assignment_id, input_ids = excluded.input_ids, turn_id = NULL, started_at = excluded.started_at",
        )
        .bind(thread_id)
        .bind(assignment_id)
        .bind(serde_json::to_string(&merged)?)
        .bind(started_at)
        .execute(tx.conn())
        .await?;
        Ok(())
    }

    async fn set_inflight_turn(tx: &mut Tx, thread_id: &str, turn_id: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE inflight SET turn_id = ? WHERE thread_id = ?")
            .bind(turn_id)
            .bind(thread_id)
            .execute(tx.conn())
            .await?;
        Ok(())
    }

    async fn delete_inflight(tx: &mut Tx, thread_id: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM inflight WHERE thread_id = ?")
            .bind(thread_id)
            .execute(tx.conn())
            .await?;
        Ok(())
    }

    fn pm_of(ticket: &Ticket) -> Option<String> {
        ticket
            .channels
            .first()
            .map(|channel| channel.thread.clone())
    }

    fn display_name(ticket: &Ticket) -> String {
        ticket
            .execution
            .as_ref()
            .map(|e| e.name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| ticket.id.clone())
    }

    async fn fail_ticket(&self, ticket_id: &str, summary: &str) -> anyhow::Result<()> {
        let report = WorkerReport {
            state: ReportState::Failed,
            summary: summary.chars().take(MAX_INPUT_CHARS).collect(),
            ..WorkerReport::default()
        };
        let engine = Arc::clone(&self.inner.engine);
        let effects = self.inner.effects.clone();
        self.inner
            .store
            .atomic(async |tx| {
                engine.report(tx, ticket_id, report.clone()).await?;
                Self::relay_report(tx, &effects, ticket_id, &report).await
            })
            .await
    }

    /// Whether the ticket was already told of this in-flight delivery: its latest assignment is the
    /// row's and last reported blocked with the recovery note.
    async fn told_of_inflight(&self, ticket_id: &str, inflight: &Inflight) -> anyhow::Result<bool> {
        Ok(self
            .inner
            .store
            .ticket(ticket_id)
            .await?
            .as_ref()
            .and_then(latest_assignment)
            .is_some_and(|assignment| {
                assignment.id == inflight.assignment_id
                    && assignment.report.as_ref().is_some_and(|report| {
                        report.state == ReportState::Blocked
                            && report.summary.starts_with(RECOVERY_NOTE)
                    })
            }))
    }

    /// A leftover `inflight` row: the ticket waits for a person, nothing is replayed, and the row
    /// stays for inspection (decision #8). The person's retry starts a new assignment, whose launch
    /// clears it. Answers whether the ticket was told now, or had been already.
    async fn block_for_inflight(
        &self,
        ticket_id: &str,
        inflight: &Inflight,
    ) -> anyhow::Result<bool> {
        if self.told_of_inflight(ticket_id, inflight).await? {
            return Ok(false);
        }
        let report = WorkerReport {
            state: ReportState::Blocked,
            summary: format!(
                "{RECOVERY_NOTE}（指派 {}，输入 {}）",
                inflight.assignment_id,
                inflight.input_ids.join(", ")
            ),
            ..WorkerReport::default()
        };
        let engine = Arc::clone(&self.inner.engine);
        let effects = self.inner.effects.clone();
        self.inner
            .store
            .atomic(async |tx| {
                engine.report(tx, ticket_id, report.clone()).await?;
                // The person learns of it through the PM, like any blocked report.
                Self::relay_report(tx, &effects, ticket_id, &report).await
            })
            .await?;
        Ok(true)
    }

    // ---- launch ----

    async fn launch(&self, ticket_id: &str, assignment_id: &str) -> Result<(), EffectError> {
        let Some(ticket) = self.current(ticket_id, assignment_id).await? else {
            return Ok(());
        };
        if self.inner.paused.load(Ordering::SeqCst) {
            return Err(EffectError::Later("processing is paused".to_owned()));
        }
        let lane = self.lane(ticket_id);
        let _permit = lane
            .acquire_owned()
            .await
            .map_err(|err| EffectError::Failed(err.to_string()))?;
        if !self
            .inner
            .slots
            .reserve(ticket_id, Self::pm_of(&ticket).as_deref(), SlotBy::Launch)
        {
            return Err(EffectError::Later("no worker slot is free".to_owned()));
        }
        let result = self.launch_in_slot(&ticket, assignment_id).await;
        if result.is_err() {
            self.inner.slots.free(ticket_id);
        }
        result
    }

    async fn launch_in_slot(
        &self,
        ticket: &Ticket,
        assignment_id: &str,
    ) -> Result<(), EffectError> {
        let ticket_id = ticket.id.as_str();
        let engine = Arc::clone(&self.inner.engine);
        let record = match self.session(ticket_id).await? {
            Some(record) => {
                if !self.known_account(&record.account) {
                    self.fail_ticket(ticket_id, "Account/workspace differs from the saved session; explicit migration is required").await?;
                    return Ok(());
                }
                if let Some(inflight) = self.inflight_for(&record.thread_id).await? {
                    if inflight.assignment_id == assignment_id {
                        self.block_for_inflight(ticket_id, &inflight).await?;
                        self.inner.slots.free(ticket_id);
                        return Ok(());
                    }
                    // A new assignment: the person retried after inspecting, so the row of the
                    // replaced assignment goes, and nothing of it is sent again.
                    let thread_id = record.thread_id.clone();
                    self.inner
                        .store
                        .atomic(async |tx| Self::delete_inflight(tx, &thread_id).await)
                        .await?;
                }
                // A resumed ticket (a follow-up attached after it ended): its thread was listed
                // as ended for the fallback hook; it works again now.
                if let Err(err) = self.forget_ended_thread(&record.thread_id).await {
                    tracing::warn!(
                        "superagent: could not unlist the resumed thread {}: {err:#}",
                        record.thread_id
                    );
                }
                // The budget starts over with the assignment.
                let mut record = record;
                if let Ok(thread) = ThreadId::from_string(&record.thread_id) {
                    record.tokens_at_start = self.tokens_used(thread).await;
                }
                self.remember(ticket_id, &record);
                let key = session_key(ticket_id);
                let saved = record.clone();
                self.inner
                    .store
                    .atomic(async |tx| {
                        tx.kv_set(&key, &saved).await?;
                        engine
                            .assignment_started(tx, ticket_id, assignment_id)
                            .await
                    })
                    .await?;
                record
            }
            None => {
                let cwd = match self.prepare_worktree(ticket).await {
                    Ok(cwd) => cwd,
                    Err(err) => {
                        self.fail_ticket(ticket_id, &format!("准备工作目录失败：{err:#}"))
                            .await?;
                        return Ok(());
                    }
                };
                // The PM's pm-threads line may pin its group (the PM and its workers) to one
                // account; otherwise the least-loaded account of the pool takes the worker.
                let pool = match self.account_for(ticket) {
                    Some(account) => vec![account],
                    None => self.inner.config.profiles.clone(),
                };
                let Some(account) = accounts::choose(
                    &pool,
                    &self.running_by_profile(),
                    self.inner.config.max_per_profile,
                ) else {
                    return Err(EffectError::Later(
                        "every account runs its maximum of workers".to_owned(),
                    ));
                };
                let spec = ThreadSpec {
                    cwd: cwd.clone(),
                    profile: Some(account.clone()),
                    model: self.inner.config.model.clone(),
                    role: Some(self.inner.config.worker_role.clone()),
                    developer_instructions: None,
                };
                let thread = match self.inner.host.start_thread(spec).await {
                    Ok(thread) => thread,
                    Err(err) => {
                        self.fail_ticket(ticket_id, &format!("起不了 worker 线程：{err:#}"))
                            .await?;
                        return Ok(());
                    }
                };
                if let Err(err) = self.inner.host.bind_profile(thread, account.clone()).await {
                    tracing::warn!(
                        "superagent: could not bind profile {account} to {thread}: {err:#}"
                    );
                }
                // The budget is the executor's own count (design §6.6). A Goal would carry it
                // too, but a Goal also makes the thread continue by itself whenever it goes idle
                // (ext/goal), against the slot and report rules here.
                let tokens_at_start = self.tokens_used(thread).await;
                let record = SessionRecord {
                    account,
                    thread_id: thread.to_string(),
                    cwd: cwd.display().to_string(),
                    model: self.inner.config.model.clone(),
                    turns: 0,
                    tokens_at_start,
                };
                let key = session_key(ticket_id);
                let saved = record.clone();
                self.inner
                    .store
                    .atomic(async |tx| {
                        tx.kv_set(&key, &saved).await?;
                        engine
                            .assignment_started(tx, ticket_id, assignment_id)
                            .await
                    })
                    .await?;
                self.remember(ticket_id, &record);
                record
            }
        };
        if record.turns == 0 {
            self.deliver_now(ticket_id, assignment_id, true).await?;
        }
        Ok(())
    }

    /// The repository, base branch and worktree root of a ticket: its PM's project when the PM
    /// thread list names one (one org, several projects, one PM each), else the defaults.
    /// The account a ticket's group is pinned to (`account=` on its PM's pm-threads line).
    fn account_for(&self, ticket: &Ticket) -> Option<String> {
        let config = &self.inner.config;
        let (Some(file), Some(pm)) = (&config.pm_threads_file, Self::pm_of(ticket)) else {
            return None;
        };
        crate::pm_threads::PmThreads::from_file(file)
            .project_of(&pm)
            .and_then(|project| project.account)
    }

    /// An account workers may run under: one of the pool, or a group account from pm-threads.
    fn known_account(&self, account: &str) -> bool {
        if self.inner.config.profiles.iter().any(|p| p == account) {
            return true;
        }
        self.inner
            .config
            .pm_threads_file
            .as_ref()
            .is_some_and(|file| {
                crate::pm_threads::PmThreads::from_file(file)
                    .accounts()
                    .iter()
                    .any(|a| a == account)
            })
    }

    fn project_for(&self, ticket: &Ticket) -> (PathBuf, String, PathBuf) {
        let config = &self.inner.config;
        let defaults = (
            config.repo.clone(),
            config.base_branch.clone(),
            config.worktree_root.clone(),
        );
        let (Some(file), Some(pm)) = (&config.pm_threads_file, Self::pm_of(ticket)) else {
            return defaults;
        };
        match crate::pm_threads::PmThreads::from_file(file).project_of(&pm) {
            Some(project) => (
                project.repo.unwrap_or(defaults.0),
                project.base_branch.unwrap_or(defaults.1),
                project.worktree_root.unwrap_or(defaults.2),
            ),
            None => defaults,
        }
    }

    async fn prepare_worktree(&self, ticket: &Ticket) -> anyhow::Result<PathBuf> {
        let name = Self::display_name(ticket);
        let (repo, base_branch, worktree_root) = self.project_for(ticket);
        let path = worktree_root.join(&name);
        if path.exists() || !self.inner.config.create_worktrees {
            return Ok(path);
        }
        let output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "add", "-b"])
            .arg(format!("superagent/{name}"))
            .arg(&path)
            .arg(&base_branch)
            .output()
            .await?;
        if !output.status.success() {
            anyhow::bail!(
                "git worktree add: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(path)
    }

    // ---- delivery ----

    async fn delivery(&self, ticket_id: &str) -> Result<(), EffectError> {
        let Some(ticket) = self.inner.store.ticket(ticket_id).await? else {
            return Ok(());
        };
        if ticket.ended.is_some() || ticket.undelivered.is_empty() {
            return Ok(());
        }
        let Some(assignment) =
            latest_assignment(&ticket).filter(|a| a.state == AssignmentState::Running)
        else {
            return Ok(());
        };
        if self.inner.paused.load(Ordering::SeqCst) {
            return Err(EffectError::Later("processing is paused".to_owned()));
        }
        let assignment_id = assignment.id.clone();
        let lane = self.lane(ticket_id);
        let _permit = lane
            .acquire_owned()
            .await
            .map_err(|err| EffectError::Failed(err.to_string()))?;
        let by = if Self::human_input(&ticket, &ticket.undelivered) {
            SlotBy::Person
        } else {
            SlotBy::Wake
        };
        if !self
            .inner
            .slots
            .reserve(ticket_id, Self::pm_of(&ticket).as_deref(), by)
        {
            return Err(EffectError::Later(
                "no worker slot is free for the wake".to_owned(),
            ));
        }
        self.deliver_now(ticket_id, &assignment_id, false).await
    }

    fn human_input(ticket: &Ticket, ids: &[String]) -> bool {
        ticket
            .inputs
            .iter()
            .any(|input| ids.iter().any(|id| id == input.id()) && input.human_own())
    }

    async fn deliver_now(
        &self,
        ticket_id: &str,
        assignment_id: &str,
        initial: bool,
    ) -> Result<(), EffectError> {
        let Some(ticket) = self.current(ticket_id, assignment_id).await? else {
            return Ok(());
        };
        let ids = ticket.undelivered.clone();
        if !initial && ids.is_empty() {
            return Ok(());
        }
        let Some(mut record) = self.session(ticket_id).await? else {
            return Err(EffectError::Failed(
                "the ticket has no worker session".to_owned(),
            ));
        };
        let thread = ThreadId::from_string(&record.thread_id).map_err(|err| {
            EffectError::Failed(format!("bad thread id {}: {err}", record.thread_id))
        })?;
        let text = self.compose(&ticket, &ids, initial).await?;
        let thread_key = record.thread_id.clone();
        let started_at = iso(self.now());
        // Persisted before the delivery: a crash between sending and recording must not replay.
        self.inner
            .store
            .atomic(async |tx| {
                Self::write_inflight(tx, &thread_key, assignment_id, &ids, &started_at).await
            })
            .await?;
        // A cold worker comes back under the current role file (design §6.7).
        let role = Some(self.inner.config.worker_role.clone());
        if let Err(err) = self.inner.host.resume_thread(thread, role).await {
            tracing::warn!("superagent: resume of {thread} failed: {err:#}");
        }
        let mode =
            if !initial && self.turn_running(&record.thread_id) && Self::human_input(&ticket, &ids)
            {
                DeliveryMode::Steer
            } else {
                DeliveryMode::Queue
            };
        let engine = Arc::clone(&self.inner.engine);
        match self.inner.host.deliver(thread, text, mode).await {
            Ok(()) => {
                record.turns += 1;
                let key = session_key(ticket_id);
                self.remember(ticket_id, &record);
                self.inner
                    .store
                    .atomic(async |tx| {
                        tx.kv_set(&key, &record).await?;
                        engine
                            .inputs_delivered(tx, ticket_id, assignment_id, &ids)
                            .await
                    })
                    .await?;
                Ok(())
            }
            Err(err) => {
                let error = format!("{err:#}");
                self.inner
                    .store
                    .atomic(async |tx| {
                        Self::delete_inflight(tx, &thread_key).await?;
                        engine
                            .delivery_failed(tx, ticket_id, assignment_id, &error)
                            .await
                    })
                    .await?;
                Err(EffectError::Failed(error))
            }
        }
    }

    /// The message a worker receives: the first assignment, or the waiting inputs (design §6.1, §7.1).
    async fn compose(
        &self,
        ticket: &Ticket,
        ids: &[String],
        initial: bool,
    ) -> anyhow::Result<String> {
        let mut lines = Vec::new();
        if initial {
            lines.push(format!(
                "工单 {}：{}",
                Self::display_name(ticket),
                ticket.title
            ));
            lines.push(format!("授权范围：{}", ticket.mandate.scope));
            if let Some(execution) = &ticket.execution {
                lines.push(format!(
                    "任务：{}",
                    truncate(&execution.assignment, MAX_ASSIGNMENT_CHARS)
                ));
            }
            if let Some(trigger) = self.inner.store.trigger(&ticket.origin.trigger_id).await? {
                lines.push(format!(
                    "原话（{}）：{}",
                    trigger.id,
                    truncate(&trigger.text, MAX_INPUT_CHARS)
                ));
            }
            lines.extend(
                self.handed_back_context(&ticket.origin.trigger_id)
                    .await?,
            );
            lines.push("先用 superagent_ack 确认要做什么，再动手；交报告用 superagent_report（五行：改动、测试、风险、证据、结论）。".to_owned());
        }
        for input in ticket
            .inputs
            .iter()
            .filter(|input| ids.iter().any(|id| id == input.id()))
        {
            lines.push(self.render_input(input).await?);
        }
        Ok(lines.join("\n"))
    }

    /// What the tickets that handed the origin message back knew: scope, task, directory, original
    /// request, latest report and evidence. A worker opened from a handback starts with none of it,
    /// and the PM's note does not reliably carry it (11/03: a new worker changed the wrong code twice).
    async fn handed_back_context(&self, trigger_id: &str) -> anyhow::Result<Vec<String>> {
        let mut lines = Vec::new();
        let Some(disposition) = self.inner.store.disposition(trigger_id).await? else {
            return Ok(lines);
        };
        let mut seen: Vec<&str> = Vec::new();
        for handback in disposition.handbacks.iter().flatten() {
            if seen.contains(&handback.ticket_id.as_str()) {
                continue;
            }
            seen.push(&handback.ticket_id);
            let Some(origin) = self.inner.store.ticket(&handback.ticket_id).await? else {
                continue;
            };
            lines.push(format!(
                "退回自工单 {}（理由：{}）。它知道的上下文，接着用：",
                Self::display_name(&origin),
                truncate(&handback.reason, MAX_ASSIGNMENT_CHARS)
            ));
            lines.push(format!("  - 原工单授权范围：{}", origin.mandate.scope));
            if let Some(execution) = &origin.execution {
                if !execution.assignment.is_empty() {
                    lines.push(format!(
                        "  - 原工单任务：{}",
                        truncate(&execution.assignment, MAX_ASSIGNMENT_CHARS)
                    ));
                }
                if !execution.directory.is_empty() {
                    lines.push(format!("  - 原工单目录：{}", execution.directory));
                }
            }
            if let Some(trigger) = self.inner.store.trigger(&origin.origin.trigger_id).await? {
                lines.push(format!(
                    "  - 原工单的原请求（{}）：{}",
                    trigger.id,
                    truncate(&trigger.text, MAX_INPUT_CHARS)
                ));
            }
            if let Some(report) = latest_assignment(&origin).and_then(|a| a.report.as_ref()) {
                lines.push(format!(
                    "  - 原工单最近报告：{}",
                    truncate(&report.summary, MAX_ASSIGNMENT_CHARS)
                ));
            }
            let details: Option<crate::cards::ReportDetails> = self
                .inner
                .store
                .kv_get(&format!("report-details:{}:latest", origin.id))
                .await?;
            if let Some(details) = details
                && !details.evidence.is_empty()
            {
                lines.push(format!(
                    "  - 原工单证据：{}",
                    details
                        .evidence
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("、")
                ));
            }
        }
        Ok(lines)
    }

    async fn render_input(&self, input: &TicketInput) -> anyhow::Result<String> {
        Ok(match input {
            TicketInput::Trigger {
                id,
                trigger_id,
                note,
                context,
                ..
            } => {
                let text = self
                    .inner
                    .store
                    .trigger(trigger_id)
                    .await?
                    .map(|t| truncate(&t.text, MAX_INPUT_CHARS))
                    .unwrap_or_default();
                let tag = if *context == Some(true) {
                    "上下文"
                } else {
                    "输入"
                };
                match note {
                    Some(note) => format!("[{tag} {id}] {text}\n（PM 备注：{note}）"),
                    None => format!("[{tag} {id}] {text}"),
                }
            }
            TicketInput::Resumed { id, trigger_id, .. } => {
                format!("[已恢复 {id}] 这张工单因你的消息 {trigger_id} 恢复；授权不变。")
            }
            TicketInput::PolicyChanged { id, .. } => {
                format!("[规则变化 {id}] 授权规则变了，重看范围。")
            }
            TicketInput::Reopen { id, .. } => format!("[重开 {id}]"),
            TicketInput::Text { id, text, .. } | TicketInput::Note { id, text, .. } => {
                format!("[备注 {id}] {}", truncate(text, MAX_INPUT_CHARS))
            }
        })
    }

    // ---- the worker's turns and reports ----

    /// A turn started on a thread: the inputs it carries are read.
    pub async fn turn_started(&self, thread_id: &str, turn_id: &str) -> anyhow::Result<()> {
        let Some(ticket_id) = self.ticket_of_thread(thread_id) else {
            return Ok(());
        };
        let inflight = self.inflight_for(thread_id).await?;
        let assignment_id = match &inflight {
            Some(inflight) => inflight.assignment_id.clone(),
            None => self
                .inner
                .store
                .ticket(&ticket_id)
                .await?
                .and_then(|t| latest_assignment(&t).map(|a| a.id.clone()))
                .unwrap_or_default(),
        };
        self.inner
            .turns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                thread_id.to_owned(),
                TurnState {
                    ticket_id: ticket_id.clone(),
                    assignment_id: assignment_id.clone(),
                    turn_id: turn_id.to_owned(),
                    capacity_failed: false,
                    reported: None,
                    question: false,
                },
            );
        let input_ids = inflight.as_ref().map(|inflight| inflight.input_ids.clone());
        let engine = Arc::clone(&self.inner.engine);
        let thread_id = thread_id.to_owned();
        let turn_id = turn_id.to_owned();
        self.inner
            .store
            .atomic(async |tx| {
                if input_ids.is_some() {
                    Self::set_inflight_turn(tx, &thread_id, &turn_id).await?;
                }
                engine.turn_started(tx, &ticket_id, &assignment_id).await?;
                if let Some(input_ids) = input_ids {
                    engine.inputs_read(tx, &ticket_id, &input_ids).await?;
                }
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// A turn ended: the delivery is over, and the turn-end rules decide the slot and the report.
    pub async fn turn_ended(&self, thread_id: &str) -> anyhow::Result<()> {
        let Some(state) = self
            .inner
            .turns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(thread_id)
        else {
            return Ok(());
        };
        let engine = Arc::clone(&self.inner.engine);
        let thread_key = thread_id.to_owned();
        let (ticket_id, assignment_id) = (state.ticket_id.clone(), state.assignment_id.clone());
        self.inner
            .store
            .atomic(async |tx| {
                Self::delete_inflight(tx, &thread_key).await?;
                engine.turn_ended(tx, &ticket_id, &assignment_id).await
            })
            .await?;
        let thread = ThreadId::from_string(thread_id).ok();
        let handed_off = matches!(
            state.reported,
            Some(
                ReportState::Ready
                    | ReportState::Blocked
                    | ReportState::Failed
                    | ReportState::Dropped
            )
        ) || state.question;
        if !handed_off {
            if let Some(reason) = self.over_budget(thread, &ticket_id).await {
                self.report_from_worker(
                    thread_id,
                    WorkerReport {
                        state: ReportState::Blocked,
                        summary: reason,
                        ..WorkerReport::default()
                    },
                )
                .await
                .map_err(|err| anyhow::anyhow!(err))?;
                return Ok(());
            }
            let goal = match thread {
                Some(thread) => self.inner.host.goal_status(thread).await.ok().flatten(),
                None => None,
            };
            if goal.as_ref().is_some_and(GoalStatus::exhausted) {
                self.report_from_worker(
                    thread_id,
                    WorkerReport {
                        state: ReportState::Blocked,
                        summary: "预算用完 / 额度用完：worker 停下，等人加预算或换账号。"
                            .to_owned(),
                        ..WorkerReport::default()
                    },
                )
                .await
                .map_err(|err| anyhow::anyhow!(err))?;
                return Ok(());
            }
        }
        let background = match thread {
            Some(thread) => self
                .inner
                .host
                .background_commands(thread)
                .await
                .unwrap_or_default(),
            None => Vec::new(),
        };
        match background::after_turn(state.reported, state.question, !background.is_empty()) {
            TurnEnd::KeepSlot => self.watch_background(thread_id, &state.ticket_id, background),
            TurnEnd::ReportWaiting => {
                self.report_from_worker(
                    thread_id,
                    WorkerReport {
                        state: ReportState::Waiting,
                        summary: "这一轮结束，没有交报告；worker 等下一条输入。".to_owned(),
                        ..WorkerReport::default()
                    },
                )
                .await
                .map_err(|err| anyhow::anyhow!(err))?;
                self.inner.slots.free(&state.ticket_id);
                if state.capacity_failed {
                    self.schedule_continue(thread_id, &state).await;
                }
            }
            TurnEnd::Handoff | TurnEnd::Idle => self.inner.slots.free(&state.ticket_id),
        }
        Ok(())
    }

    /// Tells the PM what the worker concluded, in the transaction that records the report: a
    /// question at once, a result or a stop once; ordinary progress and a plain wait stay on the
    /// ticket (prototype `notices.ts` `reportRelay`). The relay's id is the report's, so the same
    /// conclusion reaches the PM once, also across a restart (A0 gap T-08).
    async fn relay_report(
        tx: &mut Tx,
        effects: &Effects,
        ticket_id: &str,
        report: &WorkerReport,
    ) -> anyhow::Result<()> {
        let Some(ticket) = tx.ticket(ticket_id).await? else {
            return Ok(());
        };
        let state = serde_json::to_value(report.state)?
            .as_str()
            .unwrap_or("progress")
            .to_owned();
        let details = crate::cards::ReportDetails {
            changes: report.changes.clone(),
            tests_run: report.tests_run.clone(),
            risks: report.risks.clone(),
            evidence: report.evidence.clone(),
        };
        if !details.is_empty() {
            let assignment = latest_assignment(&ticket)
                .map(|a| a.id.clone())
                .unwrap_or_default();
            tx.kv_set(
                &format!("report-details:{ticket_id}:{assignment}"),
                &details,
            )
            .await?;
            tx.kv_set(&format!("report-details:{ticket_id}:latest"), &details)
                .await?;
        }
        let relay = match (&ticket.attention, report.state) {
            (Some(attention), _) if report.question.is_some() => {
                crate::cards::worker_attention_relay(&ticket, &attention.id, &state)
            }
            (_, ReportState::Progress | ReportState::Waiting) => return Ok(()),
            _ => crate::cards::worker_report_relay(
                &ticket,
                &state,
                &report.summary,
                report.answers.first().map(String::as_str),
                Some(&details),
            ),
        };
        // The relay goes to the ticket's PM (one org, several PMs).
        effects
            .enqueue(
                tx,
                "relay",
                &relay.id,
                &json!({ "text": relay.text, "pm": Self::pm_of(&ticket) }),
            )
            .await
    }

    /// Records the end of background commands as a note Input of the ticket; the engine's delivery
    /// then wakes the worker when it is idle (design §1.4).
    async fn background_ended(&self, ticket_id: &str, labels: &[String]) {
        let engine = Arc::clone(&self.inner.engine);
        let text = format!(
            "后台命令结束：{}。输出在这个线程的后台终端记录里；先读结果，再决定下一步。",
            labels.join("、")
        );
        let recorded = self
            .inner
            .store
            .atomic(async |tx| engine.background_ended(tx, ticket_id, &text).await)
            .await;
        if let Err(err) = recorded {
            tracing::warn!("superagent: background end of {ticket_id} not recorded: {err:#}");
        }
    }

    /// Watches a worker's background commands after its turn: each one that ends becomes an input
    /// of the ticket, which wakes the worker (design §1.4). At the limit the rest are killed and the
    /// ticket waits.
    ///
    /// The extension API has no event for the end of a background command: `ExecCommandEnd` for a
    /// unified-exec process is emitted by `core/src/unified_exec/async_watcher.rs` (`spawn_exit_watcher`)
    /// straight to the thread's event stream, and `TurnLifecycleContributor` / `ToolLifecycleContributor`
    /// see only turns and tool calls. Until the API exposes it, the host is polled through
    /// `background_commands`; `TODO(superagent)`: switch to the event once one exists.
    fn watch_background(&self, thread_id: &str, ticket_id: &str, running: Vec<String>) {
        let executor = self.clone();
        let thread_id = thread_id.to_owned();
        let ticket_id = ticket_id.to_owned();
        tokio::spawn(async move {
            let Ok(thread) = ThreadId::from_string(&thread_id) else {
                return;
            };
            let deadline = tokio::time::Instant::now() + executor.inner.config.background_limit;
            let poll = BACKGROUND_POLL.min(executor.inner.config.background_limit / 4);
            // The commands seen at the turn's end are the baseline: one that ends before the first
            // poll is still noticed.
            let mut known = running;
            loop {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    break;
                }
                tokio::time::sleep(poll.min(deadline - now)).await;
                if executor.turn_running(&thread_id) || !executor.inner.slots.holds(&ticket_id) {
                    return;
                }
                let current = executor
                    .inner
                    .host
                    .background_commands(thread)
                    .await
                    .unwrap_or_default();
                let finished: Vec<String> = known
                    .iter()
                    .filter(|label| !current.contains(label))
                    .cloned()
                    .collect();
                if !finished.is_empty() {
                    executor.background_ended(&ticket_id, &finished).await;
                }
                known = current;
                if known.is_empty() {
                    executor.inner.slots.free(&ticket_id);
                    return;
                }
            }
            if executor.turn_running(&thread_id) || !executor.inner.slots.holds(&ticket_id) {
                return;
            }
            let killed = executor
                .inner
                .host
                .background_commands(thread)
                .await
                .unwrap_or_default();
            if killed.is_empty() {
                executor.inner.slots.free(&ticket_id);
                return;
            }
            if let Err(err) = executor.inner.host.clean_background(thread).await {
                tracing::warn!(
                    "superagent: could not clean the background commands of {thread}: {err:#}"
                );
            }
            let report = WorkerReport {
                state: ReportState::Waiting,
                summary: format!(
                    "后台命令超过时限被结束：{}；worker 等下一条输入。",
                    killed.join("、")
                ),
                ..WorkerReport::default()
            };
            if let Err(err) = executor.report_from_worker(&thread_id, report).await {
                tracing::warn!("superagent: background report for {thread_id} failed: {err}");
            }
            executor.inner.slots.free(&ticket_id);
        });
    }

    /// Checks a report as the worker tool would (design §6.3).
    pub fn validate_report(report: &WorkerReport) -> Result<(), String> {
        if report.summary.trim().is_empty() {
            return Err("报告要有摘要".to_owned());
        }
        if report.summary.chars().count() > MAX_INPUT_CHARS {
            return Err(format!(
                "摘要不超过 {MAX_INPUT_CHARS} 字，长证据写进证据目录，报告里只放路径"
            ));
        }
        if report.state == ReportState::Ready && report.evidence.is_empty() {
            return Err("ready 要附证据路径".to_owned());
        }
        if report.question.is_some() && report.state == ReportState::Ready {
            return Err("待决问题和 ready 不能放在同一份报告里；分开报".to_owned());
        }
        if report.question.is_some() && report.reply_to.is_none() {
            return Err("待决问题要写明回复的是哪条消息（reply_to）".to_owned());
        }
        Ok(())
    }

    /// A worker's report, from its tool or from the turn-end rules.
    pub async fn report_from_worker(
        &self,
        thread_id: &str,
        mut report: WorkerReport,
    ) -> Result<(), String> {
        let ticket_id = self
            .ticket_of_thread(thread_id)
            .ok_or_else(|| "这个线程不是工单的 worker".to_owned())?;
        Self::validate_report(&report)?;
        if let Some(reply_to) = report.reply_to.take() {
            report.reply_to = Some(self.resolve_message_id(&ticket_id, &reply_to).await);
        }
        let mut answers = Vec::new();
        for id in std::mem::take(&mut report.answers) {
            answers.push(self.resolve_message_id(&ticket_id, &id).await);
        }
        report.answers = answers;
        let thread = ThreadId::from_string(thread_id).map_err(|err| err.to_string())?;
        let children = self
            .inner
            .host
            .children(thread)
            .await
            .map_err(|err| err.to_string())?;
        if report.state == ReportState::Ready && !children.is_empty() {
            return Err(format!(
                "先收掉 helper（还有 {} 个在跑），再交 ready 报告",
                children.len()
            ));
        }
        let ticket = self
            .inner
            .store
            .ticket(&ticket_id)
            .await
            .map_err(|err| err.to_string())?
            .ok_or_else(|| "工单不存在".to_owned())?;
        // A later input must be processed before a ready report can end the ticket.
        if report.state == ReportState::Ready && !ticket.undelivered.is_empty() {
            report.state = ReportState::Progress;
        }
        let engine = Arc::clone(&self.inner.engine);
        let effects = self.inner.effects.clone();
        let stored = report.clone();
        self.inner
            .store
            .atomic(async |tx| {
                engine.report(tx, &ticket_id, stored.clone()).await?;
                Self::relay_report(tx, &effects, &ticket_id, &stored).await
            })
            .await
            .map_err(|err| format!("{err:#}"))?;
        if let Some(state) = self
            .inner
            .turns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(thread_id)
        {
            state.reported = Some(report.state);
            state.question = report.question.is_some();
        }
        let handoff = matches!(
            report.state,
            ReportState::Ready | ReportState::Blocked | ReportState::Failed | ReportState::Dropped
        ) || report.question.is_some();
        if handoff {
            self.inner.slots.free(&ticket_id);
            for child in children {
                if let Err(err) = self.inner.host.interrupt(child).await {
                    tracing::warn!("superagent: could not interrupt helper {child}: {err:#}");
                }
            }
        }
        Ok(())
    }

    /// The worker acknowledged a person's request.
    /// A worker names messages by the id it sees: the `[输入 <id>]` card shows the ticket input's
    /// id, the assignment card the trigger's. Both are accepted; an input id resolves to its trigger.
    async fn resolve_message_id(&self, ticket_id: &str, id: &str) -> String {
        let Ok(Some(ticket)) = self.inner.store.ticket(ticket_id).await else {
            return id.to_owned();
        };
        ticket
            .inputs
            .iter()
            .find(|input| input.id() == id)
            .and_then(|input| input.trigger_id().map(str::to_owned))
            .unwrap_or_else(|| id.to_owned())
    }

    pub async fn ack_from_worker(
        &self,
        thread_id: &str,
        trigger_id: &str,
        text: &str,
    ) -> Result<(), String> {
        let ticket_id = self
            .ticket_of_thread(thread_id)
            .ok_or_else(|| "这个线程不是工单的 worker".to_owned())?;
        if text.split_whitespace().count() > 100 || text.chars().count() > 300 {
            return Err("确认不超过 100 词，一句话".to_owned());
        }
        let trigger_id = self.resolve_message_id(&ticket_id, trigger_id).await;
        let engine = Arc::clone(&self.inner.engine);
        self.inner
            .store
            .atomic(async |tx| engine.acknowledge(tx, &ticket_id, &trigger_id, text).await)
            .await
            .map_err(|err| format!("{err:#}"))
    }

    /// The worker hands a message back as beyond the ticket.
    pub async fn handback_from_worker(
        &self,
        thread_id: &str,
        trigger_id: &str,
        reason: &str,
    ) -> Result<bool, String> {
        let ticket_id = self
            .ticket_of_thread(thread_id)
            .ok_or_else(|| "这个线程不是工单的 worker".to_owned())?;
        let trigger_id = self.resolve_message_id(&ticket_id, trigger_id).await;
        let engine = Arc::clone(&self.inner.engine);
        self.inner
            .store
            .atomic(async |tx| engine.handback(tx, &ticket_id, &trigger_id, reason).await)
            .await
            .map_err(|err| format!("{err:#}"))
    }

    // ---- stop and restore ----

    /// Ends a ticket's work (design §6.5): the turn, the background commands, every subagent, the
    /// Goal; and lists the thread for the fallback hook.
    pub async fn stop_work(&self, ticket_id: &str, reason: &str) {
        self.inner.slots.free(ticket_id);
        let Ok(Some(record)) = self.session(ticket_id).await else {
            return;
        };
        self.inner
            .turns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&record.thread_id);
        // The delivery this row marked is over with the work; a leftover reads as in flight at the
        // next reconciliation.
        let thread_key = record.thread_id.clone();
        if let Err(err) = self
            .inner
            .store
            .atomic(async |tx| Self::delete_inflight(tx, &thread_key).await)
            .await
        {
            tracing::warn!("superagent: clearing the inflight row of {thread_key} failed: {err:#}");
        }
        let Ok(thread) = ThreadId::from_string(&record.thread_id) else {
            return;
        };
        let host = &self.inner.host;
        if let Err(err) = host.interrupt(thread).await {
            tracing::warn!("superagent: interrupt of {thread} failed: {err:#}");
        }
        if let Err(err) = host.clean_background(thread).await {
            tracing::warn!(
                "superagent: cleaning the background commands of {thread} failed: {err:#}"
            );
        }
        for child in host.children(thread).await.unwrap_or_default() {
            if let Err(err) = host.interrupt(child).await {
                tracing::warn!("superagent: interrupt of helper {child} failed: {err:#}");
            }
        }
        // A retry ends the assignment, not the ticket: the same thread goes on with its budget
        // and its tools (design §6.6), so it is neither listed as ended nor stripped of its goal.
        let ticket_open = self
            .inner
            .store
            .ticket(ticket_id)
            .await
            .ok()
            .flatten()
            .is_some_and(|ticket| ticket.ended.is_none());
        if ticket_open {
            tracing::info!(
                "superagent: ended the assignment of {ticket_id} ({reason}); its thread goes on"
            );
            return;
        }
        if let Err(err) = host.goal_clear(thread).await {
            tracing::warn!("superagent: clearing the goal of {thread} failed: {err:#}");
        }
        if let Err(err) = self.record_ended_thread(&record.thread_id).await {
            tracing::warn!(
                "superagent: could not record the ended thread {}: {err:#}",
                record.thread_id
            );
        }
        tracing::info!("superagent: stopped the work of {ticket_id} ({reason})");
    }

    async fn record_ended_thread(&self, thread_id: &str) -> anyhow::Result<()> {
        let path = &self.inner.config.ended_threads_file;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let existing = tokio::fs::read_to_string(path).await.unwrap_or_default();
        if existing.lines().any(|line| line == thread_id) {
            return Ok(());
        }
        tokio::fs::write(path, format!("{existing}{thread_id}\n")).await?;
        Ok(())
    }

    /// The thread works again (its ticket resumed): off the ended list.
    async fn forget_ended_thread(&self, thread_id: &str) -> anyhow::Result<()> {
        let path = &self.inner.config.ended_threads_file;
        let existing = match tokio::fs::read_to_string(path).await {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        if !existing.lines().any(|line| line == thread_id) {
            return Ok(());
        }
        let kept: String = existing
            .lines()
            .filter(|line| *line != thread_id)
            .map(|line| format!("{line}\n"))
            .collect();
        tokio::fs::write(path, kept).await?;
        Ok(())
    }

    /// The thread's token count so far; 0 when the host keeps none.
    async fn tokens_used(&self, thread: ThreadId) -> i64 {
        match self.inner.host.tokens_used(thread).await {
            Ok(count) => count.unwrap_or_default(),
            Err(err) => {
                tracing::warn!("superagent: could not read the token count of {thread}: {err:#}");
                0
            }
        }
    }

    /// The reason when the assignment's token use went past the budget (design §6.6), else None.
    async fn over_budget(&self, thread: Option<ThreadId>, ticket_id: &str) -> Option<String> {
        let budget = self.inner.config.token_budget?;
        let used = self
            .inner
            .host
            .tokens_used(thread?)
            .await
            .ok()
            .flatten()?;
        let start = self
            .session(ticket_id)
            .await
            .ok()
            .flatten()
            .map(|record| record.tokens_at_start)
            .unwrap_or_default();
        let spent = used.saturating_sub(start);
        (spent > budget).then(|| {
            format!(
                "预算用完：这次指派已用 {spent} token，预算 {budget}；worker 停下，等人加预算（重试）或换账号。"
            )
        })
    }

    /// At start, oldest ticket first (design §8.1): a pending assignment launches again unless its
    /// ticket waits for a person; a running one is blocked when an `inflight` row says a delivery
    /// never saw its end (decision #8: nothing is replayed, the person decides), and otherwise
    /// handed to the engine as interrupted, which restarts it with a recovery note unless it had
    /// settled. Running it twice changes nothing: a ticket already told stays told.
    pub async fn restore(&self) -> anyhow::Result<RestoreReport> {
        let mut report = RestoreReport::default();
        // Delivery rows of ended tickets are over with the work; before stop_work cleared them
        // (11/03) they lingered and read as in flight at every check.
        for ticket in self.inner.store.tickets().await? {
            if ticket.ended.is_none() {
                continue;
            }
            let Some(record) = self.session(&ticket.id).await? else {
                continue;
            };
            let thread_id = record.thread_id.clone();
            self.inner
                .store
                .atomic(async |tx| Self::delete_inflight(tx, &thread_id).await)
                .await?;
        }
        let mut tickets = self.inner.store.tickets_active().await?;
        // The store lists newest first.
        tickets.reverse();
        for ticket in tickets {
            if ticket.ended.is_some() {
                continue;
            }
            let Some(assignment) = latest_assignment(&ticket).cloned() else {
                continue;
            };
            let name = Self::display_name(&ticket);
            match assignment.state {
                AssignmentState::Pending => {
                    if waits_for_person(&ticket) {
                        report.waiting.push(name);
                        continue;
                    }
                    self.inner
                        .effects
                        .enqueue_now(
                            KIND_LAUNCH,
                            &assignment.id,
                            &json!({ "ticketId": ticket.id, "assignmentId": assignment.id }),
                        )
                        .await?;
                    report.relaunched.push(name);
                }
                AssignmentState::Running => {
                    let Some(record) = self.session(&ticket.id).await? else {
                        report.untouched += 1;
                        continue;
                    };
                    self.remember(&ticket.id, &record);
                    if let Some(inflight) = self.inflight_for(&record.thread_id).await? {
                        if self.block_for_inflight(&ticket.id, &inflight).await? {
                            report.blocked.push(name);
                        } else {
                            report.untouched += 1;
                        }
                    } else {
                        let engine = Arc::clone(&self.inner.engine);
                        let (ticket_id, assignment_id) = (ticket.id.clone(), assignment.id.clone());
                        let before = ticket.assignments.len();
                        self.inner
                            .store
                            .atomic(async |tx| {
                                engine
                                    .assignment_interrupted(tx, &ticket_id, &assignment_id)
                                    .await
                            })
                            .await?;
                        let restarted = self
                            .inner
                            .store
                            .ticket(&ticket.id)
                            .await?
                            .is_some_and(|t| t.assignments.len() > before);
                        if restarted {
                            report.restarted.push(name);
                        } else {
                            report.untouched += 1;
                        }
                    }
                }
                AssignmentState::Ended => report.untouched += 1,
            }
        }
        Ok(report)
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}…（共 {} 字）", text.chars().count())
}

impl Executor for CodextThreadExecutor {
    fn start<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket: &'a Ticket,
        assignment: &'a Assignment,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(async move {
            self.inner
                .effects
                .enqueue(
                    tx,
                    KIND_LAUNCH,
                    &assignment.id,
                    &json!({ "ticketId": ticket.id, "assignmentId": assignment.id }),
                )
                .await
        })
    }

    fn deliver<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket: &'a Ticket,
        _assignment: &'a Assignment,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(async move {
            self.inner
                .effects
                .enqueue(
                    tx,
                    KIND_DELIVERY,
                    &ticket.id,
                    &json!({ "ticketId": ticket.id }),
                )
                .await
        })
    }

    fn stop<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket: &'a Ticket,
        _assignment: &'a Assignment,
        reason: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(async move {
            // Only once the end committed: a rolled-back end must not stop the worker.
            let executor = self.clone();
            let ticket_id = ticket.id.clone();
            let reason = reason.to_owned();
            tx.after_commit(move || {
                tokio::spawn(async move { executor.stop_work(&ticket_id, &reason).await });
            });
            Ok(())
        })
    }
}

/// Registers the `launch` and `delivery` handlers. What was running is restored by
/// `crate::recovery::reconcile`, once the engine's hooks are in place.
pub async fn install_executor(
    store: Store,
    effects: Effects,
    engine: Arc<dyn EngineForExecutor>,
    host: Arc<dyn SuperagentHost>,
    config: ExecutorConfig,
    clock: Clock,
) -> anyhow::Result<CodextThreadExecutor> {
    let executor = CodextThreadExecutor::new(store, effects.clone(), engine, host, config, clock);
    let launcher = executor.clone();
    let slots = executor.clone();
    effects
        .handle(
            KIND_LAUNCH,
            Arc::new(move |payload, _key| {
                let launcher = launcher.clone();
                Box::pin(async move {
                    let ticket_id = payload["ticketId"].as_str().unwrap_or_default().to_owned();
                    let assignment_id = payload["assignmentId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    launcher.launch(&ticket_id, &assignment_id).await
                })
            }),
            HandlerOptions::default()
                .limit(move || Ok(slots.inner.slots.launch_room().max(1)))
                .retry(5_000, 300_000),
        )
        .await?;
    let deliverer = executor.clone();
    effects
        .handle(
            KIND_DELIVERY,
            Arc::new(move |payload, _key| {
                let deliverer = deliverer.clone();
                Box::pin(async move {
                    let ticket_id = payload["ticketId"].as_str().unwrap_or_default().to_owned();
                    deliverer.delivery(&ticket_id).await
                })
            }),
            HandlerOptions::default()
                .limit(|| Ok(4))
                .retry(2_000, 60_000)
                .quiet(),
        )
        .await?;
    let continuer = executor.clone();
    effects
        .handle(
            KIND_CONTINUE,
            Arc::new(move |payload, _key| {
                let continuer = continuer.clone();
                Box::pin(async move {
                    let Ok(continuation) = serde_json::from_value::<capacity::Continuation>(payload) else {
                        // build-27 rows lack a failed-turn receipt; never replay them blindly.
                        tracing::warn!("superagent: discarded an unverified legacy continuation");
                        return Ok(());
                    };
                    continuer.continue_after_incident(continuation).await
                })
            }),
            HandlerOptions::default()
                .limit(|| Ok(2))
                .retry(30_000, 300_000)
                .quiet(),
        )
        .await?;
    let restorer = executor.clone();
    effects
        .handle(
            KIND_MODEL_RESTORE,
            Arc::new(move |payload, _key| {
                let restorer = restorer.clone();
                Box::pin(async move {
                    let thread_id = payload["threadId"].as_str().unwrap_or_default().to_owned();
                    restorer.restore_model(&thread_id).await
                })
            }),
            HandlerOptions::default()
                .limit(|| Ok(2))
                .retry(60_000, 600_000)
                .quiet(),
        )
        .await?;
    // capacity incidents feed the fallback policy (design §6.8)
    let observer = executor.clone();
    crate::incidents::subscribe(Arc::new(move |incident: &crate::incidents::Incident| {
        let observer = observer.clone();
        let thread = incident.thread.clone();
        tokio::spawn(async move { observer.on_incident(&thread).await });
    }));
    Ok(executor)
}

/// Feeds the worker threads' turn starts and ends to the executor.
pub struct ExecutorLifecycle {
    executor: Arc<dyn Fn() -> Option<CodextThreadExecutor> + Send + Sync>,
}

impl ExecutorLifecycle {
    pub fn new(executor: Arc<dyn Fn() -> Option<CodextThreadExecutor> + Send + Sync>) -> Self {
        Self { executor }
    }
}

impl TurnLifecycleContributor for ExecutorLifecycle {
    fn on_turn_start<'a>(&'a self, input: TurnStartInput<'a>) -> ExtensionFuture<'a, ()> {
        let executor = (self.executor)();
        let thread_id = input.thread_store.level_id().to_string();
        let turn_id = input.turn_id.to_owned();
        Box::pin(async move {
            if let Some(executor) = executor
                && let Err(err) = executor.turn_started(&thread_id, &turn_id).await
            {
                tracing::warn!("superagent: turn start of {thread_id} not recorded: {err:#}");
            }
        })
    }

    fn on_turn_error<'a>(&'a self, input: TurnErrorInput<'a>) -> ExtensionFuture<'a, ()> {
        let executor = (self.executor)();
        let thread_id = input.thread_store.level_id().to_string();
        let turn_id = input.turn_id.to_owned();
        let capacity_failed = matches!(
            input.error,
            codex_protocol::protocol::CodexErrorInfo::UsageLimitExceeded
                | codex_protocol::protocol::CodexErrorInfo::RateLimitExceeded
                | codex_protocol::protocol::CodexErrorInfo::ServerOverloaded
        ) || crate::incidents::classify(&input.error_details.to_string(), /*details*/ None).is_some();
        Box::pin(async move {
            if let Some(executor) = executor
                && let Some(state) = executor.inner.turns.lock()
                    .unwrap_or_else(PoisonError::into_inner).get_mut(&thread_id)
                && state.turn_id == turn_id
            {
                state.capacity_failed = capacity_failed;
            }
        })
    }

    fn on_turn_stop<'a>(&'a self, input: TurnStopInput<'a>) -> ExtensionFuture<'a, ()> {
        let executor = (self.executor)();
        let thread_id = input.thread_store.level_id().to_string();
        Box::pin(async move {
            if let Some(executor) = executor
                && let Err(err) = executor.turn_ended(&thread_id).await
            {
                tracing::warn!("superagent: turn end of {thread_id} not recorded: {err:#}");
            }
        })
    }

    fn on_turn_abort<'a>(&'a self, input: TurnAbortInput<'a>) -> ExtensionFuture<'a, ()> {
        let executor = (self.executor)();
        let thread_id = input.thread_store.level_id().to_string();
        Box::pin(async move {
            if let Some(executor) = executor {
                // An explicit interruption, replacement, or budget stop never schedules a retry.
                if let Some(state) = executor.inner.turns.lock()
                    .unwrap_or_else(PoisonError::into_inner).get_mut(&thread_id)
                {
                    state.capacity_failed = false;
                }
                if let Err(err) = executor.turn_ended(&thread_id).await {
                    tracing::warn!("superagent: turn abort of {thread_id} not recorded: {err:#}");
                }
            }
        })
    }
}
