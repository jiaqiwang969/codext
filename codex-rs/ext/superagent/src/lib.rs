//! Superagent coordination engine.
//!
//! Ported from the dsh-superagent prototype (`6337fc8`), module by module (see
//! `233-super-agent/plan/design/merge-map.md`). This crate owns `superagent_1.sqlite`: the engine is
//! its only writer, every command runs in one transaction, and the side effects a command asks for
//! are written to the `effect` table in that same transaction and executed after it commits.
//!
//! T-11 shipped the parts the later tickets build on: ids, the health board, the store with its
//! transactions and repositories, the durable effect queue, the pass scheduler and the host
//! interface the engine uses to drive codext threads.
//!
//! T-12 adds the engine core: intake, Thread rules, evals, tickets, replies, and the turn-input
//! intake that records the user's turns as Triggers.
//!
//! T-13 adds the PM side: the PM's native tools (`superagent_route`, `superagent_read`, ...), the
//! cards, and the delivery of candidate cards, relays and notices into the PM thread.
//!
//! T-14 adds the executor: one codext root thread per ticket, the worker tools and the
//! turn-lifecycle feed. `integration` glues the three onto the engine.

mod arbiter;
pub mod cards;
pub mod core;
pub mod drive;
pub mod effects;
pub mod executor;
pub mod health;
pub mod host;
pub mod ids;
pub mod incidents;
pub mod intake;
pub mod integration;
pub mod outbox;
pub mod passes;
pub mod pm;
pub mod pm_threads;
pub mod recovery;
pub mod resources;
pub mod router;
pub mod status;
pub mod store;
pub mod tools;

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_extension_api::ExtensionRegistryBuilder;
use codex_state::StateRuntime;
use tokio::sync::OnceCell;

pub use effects::EffectError;
pub use effects::Effects;
pub use effects::HandlerOptions;
pub use executor::CodextThreadExecutor;
pub use executor::EngineForExecutor;
pub use executor::ExecutorConfig;
pub use health::Health;
pub use health::HealthBoard;
pub use health::HealthState;
pub use host::SuperagentHost;
pub use store::Store;
pub use store::Tx;

/// A boxed future, as the host callbacks and the transactional closures return them.
pub type BoxFut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// The PM whose tickets the A1 engine numbers.
pub const PM: &str = "pm-codext";

/// What the extension runs once its database is open.
pub struct Runtime {
    pub store: Store,
    pub effects: Effects,
    pub passes: passes::Passes,
    pub engine: core::Engine,
    /// The executor, once a host was supplied before the start.
    pub executor: Option<CodextThreadExecutor>,
    /// What the startup reconciliation did (design §8.1), once a host was supplied.
    pub recovery: Option<recovery::Report>,
    /// The drive loop, once a host was supplied.
    pub drive: Option<drive::DriveService>,
}

/// Process-scoped service: one per app-server, created next to the queue service and handed to
/// `install`. The engine starts in the background; `runtime()` answers `None` until it has.
pub struct SuperagentService {
    state: Arc<StateRuntime>,
    board: HealthBoard,
    pm_threads: pm_threads::PmThreads,
    dir: PathBuf,
    runtime: OnceCell<Arc<Runtime>>,
    /// Serializes initialization and shutdown so recovery cannot outlive draining.
    lifecycle: tokio::sync::Mutex<()>,
    stopping: AtomicBool,
    /// The PM side, once the engine and the host are up.
    pm: OnceCell<Arc<pm::PmRuntime>>,
    host: Mutex<Option<Arc<dyn SuperagentHost>>>,
    /// Every text the engine delivered, so the intake does not read it back as a trigger.
    ledger: Arc<host::DeliveryLedger>,
    executor_config: Mutex<Option<ExecutorConfig>>,
}

impl SuperagentService {
    /// Takes the shared SQLite configuration and the extension directory under `CODEX_HOME`.
    /// The organization ledger is opened lazily when this service starts.
    pub fn new(state: Arc<StateRuntime>, dir: PathBuf) -> Self {
        incidents::install(&dir);
        Self {
            state,
            board: HealthBoard::new(),
            pm_threads: pm_threads::PmThreads::new(&dir),
            dir,
            runtime: OnceCell::new(),
            lifecycle: tokio::sync::Mutex::new(()),
            stopping: AtomicBool::new(false),
            pm: OnceCell::new(),
            host: Mutex::new(None),
            ledger: Arc::new(host::DeliveryLedger::default()),
            executor_config: Mutex::new(None),
        }
    }

    /// The app-server's implementation of the host interface; supply it before the start.
    pub fn set_host(&self, host: Arc<dyn SuperagentHost>) {
        let host: Arc<dyn SuperagentHost> =
            Arc::new(host::LedgerHost::new(host, Arc::clone(&self.ledger)));
        *self.host.lock().unwrap_or_else(PoisonError::into_inner) = Some(host);
    }

    /// What the engine delivered to threads itself; the intake skips it.
    pub fn ledger(&self) -> &host::DeliveryLedger {
        &self.ledger
    }

    /// Stop creating new effects before app-server drains and shuts down threads.
    pub async fn drain(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        let _lifecycle = self.lifecycle.lock().await;
        if let Some(runtime) = self.runtime.get() {
            runtime.passes.stop();
            runtime.effects.drain().await;
        }
    }

    pub fn set_executor_config(&self, config: ExecutorConfig) {
        *self
            .executor_config
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(config);
    }

    /// The host, once supplied.
    pub fn host(&self) -> Option<Arc<dyn SuperagentHost>> {
        self.host
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Builds the store, the effect queue, the engine and the pass scheduler on the engine's
    /// database and starts them; with a host, also the executor and the PM side. Safe to call more
    /// than once; later calls return the same runtime.
    pub async fn start(&self) -> anyhow::Result<Arc<Runtime>> {
        let _lifecycle = self.lifecycle.lock().await;
        anyhow::ensure!(!self.stopping.load(Ordering::SeqCst), "superagent is stopping");
        if let Some(runtime) = self.runtime.get() {
            return Ok(Arc::clone(runtime));
        }
        let runtime = self
            .runtime
            .get_or_try_init(|| async {
                let pool = codex_state::open_superagent_db(self.state.sqlite()).await?;
                let store = Store::new(pool);
                let effects =
                    Effects::new(store.clone(), self.board.clone(), effects::system_clock())
                        .await?;
                let engine = core::Engine::new(core::EngineOptions {
                    store: store.clone(),
                    effects: effects.clone(),
                    policy: Arc::new(core::CodextPolicy::default()),
                    scope: Arc::new(core::ListenAll),
                    hooks: Arc::new(core::NoHooks),
                    now: None,
                    prepare_ticket: None,
                    pm: PM.to_owned(),
                });
                let passes = passes::Passes::new(effects.clone(), self.board.clone())?;
                let due = engine.clone();
                passes.set_hook(Arc::new(move || {
                    let engine = due.clone();
                    Box::pin(async move { engine.evaluate_due().await })
                }));
                let (executor, recovery, drive) = match self.host() {
                    Some(host) => {
                        let config = self
                            .executor_config
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .clone()
                            .unwrap_or_else(|| ExecutorConfig::for_home(&executor_root(&self.dir)));
                        // Test knob: run workers under another role file (a pre-flight with
                        // the production role untouched). Unset in the org's service unit.
                        let config = match std::env::var("CODEXT_SUPERAGENT_WORKER_ROLE") {
                            Ok(role) if !role.trim().is_empty() => ExecutorConfig {
                                worker_role: role.trim().to_owned(),
                                ..config
                            },
                            _ => config,
                        };
                        // Ops knob: the per-assignment token budget; 0 turns it off.
                        let config = match std::env::var("CODEXT_SUPERAGENT_TOKEN_BUDGET") {
                            Ok(budget) if !budget.trim().is_empty() => ExecutorConfig {
                                token_budget: budget
                                    .trim()
                                    .parse::<i64>()
                                    .ok()
                                    .filter(|budget| *budget > 0),
                                ..config
                            },
                            _ => config,
                        };
                        // Ops knobs: the account profiles the workers run under (comma-separated)
                        // and how many workers one account carries at a time.
                        let config = match std::env::var("CODEXT_SUPERAGENT_PROFILES") {
                            Ok(list) if !list.trim().is_empty() => ExecutorConfig {
                                profiles: list
                                    .split(',')
                                    .map(str::trim)
                                    .filter(|p| !p.is_empty())
                                    .map(str::to_owned)
                                    .collect(),
                                ..config
                            },
                            _ => config,
                        };
                        // Without an explicit pool, workers run under the profile the daemon
                        // was launched with (the account launchers set it).
                        let config = match std::env::var("CODEX_ACCOUNT_PROFILE") {
                            Ok(profile)
                                if config.profiles.is_empty() && !profile.trim().is_empty() =>
                            {
                                ExecutorConfig {
                                    profiles: vec![profile.trim().to_owned()],
                                    ..config
                                }
                            }
                            _ => config,
                        };
                        let config = match std::env::var("CODEXT_SUPERAGENT_MAX_PER_PROFILE") {
                            Ok(n) if !n.trim().is_empty() => ExecutorConfig {
                                max_per_profile: n
                                    .trim()
                                    .parse::<usize>()
                                    .ok()
                                    .filter(|n| *n > 0)
                                    .unwrap_or(config.max_per_profile),
                                ..config
                            },
                            _ => config,
                        };
                        // Ops knobs: the repository the worktrees come from, its base branch and
                        // the worktree root; the org works on one project at a time.
                        let env = |name: &str| {
                            std::env::var(name)
                                .ok()
                                .map(|v| v.trim().to_owned())
                                .filter(|v| !v.is_empty())
                        };
                        let config = ExecutorConfig {
                            repo: env("CODEXT_SUPERAGENT_REPO")
                                .map(std::path::PathBuf::from)
                                .unwrap_or(config.repo),
                            base_branch: env("CODEXT_SUPERAGENT_BASE_BRANCH")
                                .unwrap_or(config.base_branch),
                            worktree_root: env("CODEXT_SUPERAGENT_WORKTREE_ROOT")
                                .map(std::path::PathBuf::from)
                                .unwrap_or(config.worktree_root),
                            ..config
                        };
                        // Capacity policy knobs (design §6.8); the fallback model turns it on.
                        let secs = |name: &str| {
                            env(name)
                                .and_then(|v| v.parse::<u64>().ok())
                                .map(std::time::Duration::from_secs)
                        };
                        let config = ExecutorConfig {
                            capacity_fallback_model: env(
                                "CODEXT_SUPERAGENT_CAPACITY_FALLBACK_MODEL",
                            ),
                            capacity_fallback_effort: env(
                                "CODEXT_SUPERAGENT_CAPACITY_FALLBACK_EFFORT",
                            )
                            .or(config.capacity_fallback_effort),
                            capacity_threshold: env("CODEXT_SUPERAGENT_CAPACITY_THRESHOLD")
                                .and_then(|v| v.parse::<usize>().ok())
                                .filter(|n| *n > 0)
                                .unwrap_or(config.capacity_threshold),
                            capacity_window: secs("CODEXT_SUPERAGENT_CAPACITY_WINDOW_SECS")
                                .unwrap_or(config.capacity_window),
                            capacity_restore: secs("CODEXT_SUPERAGENT_CAPACITY_RESTORE_SECS")
                                .unwrap_or(config.capacity_restore),
                            capacity_continue_delay: secs(
                                "CODEXT_SUPERAGENT_CAPACITY_CONTINUE_SECS",
                            )
                            .unwrap_or(config.capacity_continue_delay),
                            capacity_continue_max: env(
                                "CODEXT_SUPERAGENT_CAPACITY_CONTINUE_MAX",
                            )
                            .and_then(|v| v.parse::<u32>().ok())
                            .unwrap_or(config.capacity_continue_max),
                            ..config
                        };
                        let config = ExecutorConfig {
                            pm_threads_file: Some(self.dir.join("pm-threads")),
                            ..config
                        };
                        let executor = executor::install_executor(
                            store.clone(),
                            effects.clone(),
                            Arc::new(integration::ExecutorEngine(engine.clone())),
                            Arc::clone(&host),
                            config,
                            effects::system_clock(),
                        )
                        .await?;
                        engine.set_hooks(Arc::new(integration::ExecutorHooks {
                            executor: executor.clone(),
                            store: store.clone(),
                        }));
                        outbox::install_outbox(
                            effects.clone(),
                            store.clone(),
                            Arc::clone(&host),
                            pm::PmThreads::new(self.dir.join("pm-threads")),
                        )
                        .await?;
                        let drive = drive::DriveService::install(
                            store.clone(),
                            effects.clone(),
                            Arc::clone(&host),
                            Arc::new(drive::Oev2Judge::for_home(
                                self.dir.parent().unwrap_or(&self.dir),
                            )),
                            drive::STALL,
                        )
                        .await?;
                        let pm = Arc::new(pm::PmRuntime {
                            engine: Arc::new(integration::PmEngine(engine.clone())),
                            host,
                            store: store.clone(),
                            effects: effects.clone(),
                            pm_threads: pm::PmThreads::new(self.dir.join("pm-threads")),
                        });
                        pm::install_pm_handlers(Arc::clone(&pm)).await?;
                        let _ = self.pm.set(pm);
                        let recovery = recovery::reconcile(&store, &executor, &self.board).await?;
                        drive.recover().await?;
                        (Some(executor), Some(recovery), Some(drive))
                    }
                    None => {
                        tracing::info!(
                            "superagent: no host supplied; the executor and the PM delivery are off"
                        );
                        (None, None, None)
                    }
                };
                tracing::info!(
                    "superagent engine started on {}",
                    self.state.sqlite().superagent_db_path().display()
                );
                anyhow::Ok(Arc::new(Runtime {
                    store,
                    effects,
                    passes,
                    engine,
                    executor,
                    recovery,
                    drive,
                }))
            })
            .await?;
        // Recovery and all handlers must be ready before any persisted effect can run.
        if self.stopping.load(Ordering::SeqCst) {
            runtime.passes.stop();
            runtime.effects.drain().await;
            anyhow::bail!("superagent stopped during initialization");
        }
        runtime.effects.start();
        runtime.passes.start();
        Ok(Arc::clone(runtime))
    }

    /// The running engine, once its database is open.
    pub fn runtime(&self) -> Option<Arc<Runtime>> {
        self.runtime.get().map(Arc::clone)
    }

    /// The PM side, once the engine started with a host.
    pub fn pm(&self) -> Option<Arc<pm::PmRuntime>> {
        self.pm.get().map(Arc::clone)
    }

    /// Every part's health, by name, for `superagent/status`.
    pub fn health(&self) -> BTreeMap<String, Health> {
        self.board.all()
    }

    pub fn board(&self) -> &HealthBoard {
        &self.board
    }

    pub fn state(&self) -> &Arc<StateRuntime> {
        &self.state
    }

    pub fn pm_threads(&self) -> &pm_threads::PmThreads {
        &self.pm_threads
    }

    /// The Router's level, read from `superagent/router-mode` on each use (`router.rs`).
    pub fn router_mode(&self) -> router::RouterMode {
        router::RouterMode::read(&self.dir)
    }

    /// Whether the organization drives this thread: a PM thread, a session under drive, or the
    /// worker thread of a ticket that has not ended. Codext's own automatic drivers (daemon
    /// recovery, Goal continuation) leave such threads to the engine's reconciliation. A failed
    /// lookup answers true, so an uncertain thread is not continued twice.
    pub async fn owns_thread(&self, thread_id: &str) -> bool {
        if self.pm_threads.is_pm(thread_id) {
            return true;
        }
        let Some(runtime) = self.runtime() else {
            return false;
        };
        if let Some(drive) = &runtime.drive {
            match drive.drives(thread_id).await {
                Ok(true) => return true,
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!("superagent: could not read the drive of {thread_id}: {err:#}");
                    return true;
                }
            }
        }
        let ticket_id = match runtime.engine.worker_ticket(thread_id).await {
            Ok(Some(ticket_id)) => ticket_id,
            Ok(None) => return false,
            Err(err) => {
                tracing::warn!("superagent: could not read the owner of {thread_id}: {err:#}");
                return true;
            }
        };
        match runtime.store.ticket(&ticket_id).await {
            Ok(ticket) => ticket.is_some_and(|ticket| ticket.ended.is_none()),
            Err(err) => {
                tracing::warn!("superagent: could not read ticket {ticket_id}: {err:#}");
                true
            }
        }
    }

    /// The extension's directory under `CODEX_HOME`.
    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    /// Starts the service once the host is supplied; errors are logged.
    pub fn start_in_background(self: &Arc<Self>) {
        let service = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(err) = service.start().await {
                tracing::error!("superagent engine failed to start: {err:#}");
            }
        });
    }
}

/// The root of the organization's working files: `CODEXT_SUPERAGENT_ROOT`, else `dir` (the
/// extension's directory under `CODEX_HOME`), so an install never writes into the user's home.
pub(crate) fn executor_root(dir: &Path) -> PathBuf {
    std::env::var_os("CODEXT_SUPERAGENT_ROOT")
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.to_path_buf())
}

/// Installs the extension: the intake that records the user's turns, the PM's tools and the
/// Router's context on PM threads, the worker tools on worker threads and the turn-lifecycle feed of the executor. The
/// host supplies its `SuperagentHost`, then calls `start_in_background`.
pub fn install<C>(registry: &mut ExtensionRegistryBuilder<C>, service: Arc<SuperagentService>)
where
    C: Send + Sync + 'static,
{
    let drives = drive_lookup(&service);
    registry.turn_input_contributor(Arc::new(intake::SuperagentIntake::new(&service)));
    registry.automatic_turn_arbiter(Arc::new(arbiter::SuperagentArbiter::new(&service)));
    registry.tool_contributor(Arc::new(pm::PmExtension::new(Arc::clone(&service))));
    registry.prompt_contributor(Arc::new(router::RouterContext::new(&service)));
    let lookup = Arc::new(move || {
        service
            .runtime()
            .and_then(|runtime| runtime.executor.clone())
    }) as Arc<dyn Fn() -> Option<CodextThreadExecutor> + Send + Sync>;
    registry.tool_contributor(Arc::new(tools::worker::WorkerTools::new(Arc::clone(
        &lookup,
    ))));
    registry.turn_lifecycle_contributor(Arc::new(executor::ExecutorLifecycle::new(lookup)));
    registry.turn_lifecycle_contributor(Arc::new(drive::DriveLifecycle::new(drives)));
}

/// The drive loop of a running engine, looked up on each use.
pub(crate) fn drive_lookup(
    service: &Arc<SuperagentService>,
) -> Arc<dyn Fn() -> Option<drive::DriveService> + Send + Sync> {
    let service = Arc::downgrade(service);
    Arc::new(move || {
        service
            .upgrade()
            .and_then(|service| service.runtime())
            .and_then(|runtime| runtime.drive.clone())
    })
}

impl SuperagentService {
    /// A person's message steered into a running turn, which the turn-input contributor never
    /// sees: the host reports it and the intake records it like any other turn.
    pub async fn steered_input(&self, thread_id: &str, turn_id: &str, text: &str) {
        intake::record_turn(self, thread_id, turn_id, text, true).await;
    }
}
