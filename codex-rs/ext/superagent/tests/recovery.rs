//! Startup reconciliation and retry on the real engine and executor, with a fake host: the restart
//! cases of the prototype's `tests/runtime.spec.ts`, the replay refusal of `tests/codex.spec.ts`
//! and the T-16 acceptance of design §10.1 (the same ticket continues on the same thread, and no
//! action that went out runs twice).

mod common;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_protocol::ThreadId;
use codex_superagent_extension::BoxFut;
use codex_superagent_extension::Effects;
use codex_superagent_extension::HealthBoard;
use codex_superagent_extension::HealthState;
use codex_superagent_extension::Store;
use codex_superagent_extension::core::AcceptOutcome;
use codex_superagent_extension::core::ActorRef;
use codex_superagent_extension::core::Addressed;
use codex_superagent_extension::core::AssignmentState;
use codex_superagent_extension::core::ChannelRef;
use codex_superagent_extension::core::CodextPolicy;
use codex_superagent_extension::core::Engine;
use codex_superagent_extension::core::EngineOptions;
use codex_superagent_extension::core::ListenAll;
use codex_superagent_extension::core::NoHooks;
use codex_superagent_extension::core::Outcome;
use codex_superagent_extension::core::Report as TicketReport;
use codex_superagent_extension::core::ReportState;
use codex_superagent_extension::core::ThreadRef;
use codex_superagent_extension::core::Ticket;
use codex_superagent_extension::core::Trigger;
use codex_superagent_extension::core::latest_assignment;
use codex_superagent_extension::core::latest_report;
use codex_superagent_extension::executor::CodextThreadExecutor;
use codex_superagent_extension::executor::ExecutorConfig;
use codex_superagent_extension::executor::WorkerReport;
use codex_superagent_extension::executor::codext_thread::KIND_LAUNCH;
use codex_superagent_extension::executor::codext_thread::RECOVERY_NOTE;
use codex_superagent_extension::executor::install_executor;
use codex_superagent_extension::host::DeliveryMode;
use codex_superagent_extension::host::GoalStatus;
use codex_superagent_extension::host::SuperagentHost;
use codex_superagent_extension::host::ThreadSpec;
use codex_superagent_extension::integration::ExecutorEngine;
use codex_superagent_extension::integration::ExecutorHooks;
use codex_superagent_extension::recovery;
use common::FakeClock;
use common::T0;
use common::open_store;
use common::wait_until;
use tempfile::TempDir;

/// Records every call; threads are fresh ids.
#[derive(Default)]
struct FakeHost {
    calls: Mutex<Vec<String>>,
}

impl FakeHost {
    fn push(&self, call: String) {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(call);
    }

    fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn with(&self, prefix: &str) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| c.starts_with(prefix))
            .collect()
    }

    fn count(&self, prefix: &str) -> usize {
        self.with(prefix).len()
    }
}

impl SuperagentHost for FakeHost {
    fn start_thread(&self, spec: ThreadSpec) -> BoxFut<'_, anyhow::Result<ThreadId>> {
        self.push(format!("start_thread:{}", spec.cwd.display()));
        Box::pin(async { Ok(ThreadId::new()) })
    }
    fn bind_profile(&self, thread: ThreadId, profile: String) -> BoxFut<'_, anyhow::Result<()>> {
        self.push(format!("bind_profile:{thread}:{profile}"));
        Box::pin(async { Ok(()) })
    }
    fn resume_thread(
        &self,
        thread: ThreadId,
        _role: Option<String>,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        self.push(format!("resume:{thread}"));
        Box::pin(async { Ok(()) })
    }
    fn deliver(
        &self,
        thread: ThreadId,
        text: String,
        mode: DeliveryMode,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        let first = text.lines().next().unwrap_or_default().to_owned();
        self.push(format!("deliver:{thread}:{mode:?}:{first}"));
        Box::pin(async { Ok(()) })
    }
    fn interrupt(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        self.push(format!("interrupt:{thread}"));
        Box::pin(async { Ok(()) })
    }
    fn clean_background(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        self.push(format!("clean_background:{thread}"));
        Box::pin(async { Ok(()) })
    }
    fn background_commands(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<String>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn children(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<ThreadId>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn goal_set(
        &self,
        thread: ThreadId,
        _objective: String,
        _budget: Option<i64>,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        self.push(format!("goal_set:{thread}"));
        Box::pin(async { Ok(()) })
    }
    fn goal_clear(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        self.push(format!("goal_clear:{thread}"));
        Box::pin(async { Ok(()) })
    }
    fn goal_status(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Option<GoalStatus>>> {
        Box::pin(async { Ok(None) })
    }
}

/// The engine, the executor and the host of one process on a store; a restart builds another on
/// the same store and host.
struct World {
    store: Store,
    board: HealthBoard,
    clock: FakeClock,
    seconds: Arc<AtomicI64>,
    host: Arc<FakeHost>,
    config: ExecutorConfig,
    effects: Effects,
    engine: Engine,
    executor: CodextThreadExecutor,
    report: recovery::Report,
}

fn config(dir: &TempDir) -> ExecutorConfig {
    ExecutorConfig {
        repo: dir.path().join("repo"),
        worktree_root: dir.path().join("wt"),
        base_branch: "superagent/integration".into(),
        create_worktrees: false,
        profiles: vec!["A".into(), "B".into()],
        max_per_profile: 3,
        max_workers: 4,
        max_per_pm: 4,
        ended_threads_file: dir.path().join("state/ended-threads"),
        pm_threads_file: None,
        worker_role: "worker".into(),
        model: None,
        token_budget: Some(50_000),
        background_limit: Duration::from_millis(60),
        capacity_fallback_model: None,
        capacity_fallback_effort: Some("high".into()),
        capacity_threshold: 2,
        capacity_window: Duration::from_secs(600),
        capacity_restore: Duration::from_secs(1800),
        capacity_continue_delay: Duration::from_secs(120),
        capacity_continue_max: 3,
    }
}

fn iso(seconds: i64) -> String {
    format!("2026-09-23T00:{:02}:{:02}.000Z", seconds / 60, seconds % 60)
}

async fn boot(
    store: Store,
    clock: FakeClock,
    seconds: Arc<AtomicI64>,
    host: Arc<FakeHost>,
    config: ExecutorConfig,
) -> anyhow::Result<World> {
    let board = HealthBoard::new();
    let effects = Effects::new(store.clone(), board.clone(), clock.clock()).await?;
    effects.start();
    let now = Arc::clone(&seconds);
    let engine = Engine::new(EngineOptions {
        store: store.clone(),
        effects: effects.clone(),
        policy: Arc::new(CodextPolicy::default()),
        scope: Arc::new(ListenAll),
        hooks: Arc::new(NoHooks),
        now: Some(Arc::new(move || iso(now.load(Ordering::SeqCst)))),
        prepare_ticket: None,
        pm: "pm-test".into(),
    });
    let executor = install_executor(
        store.clone(),
        effects.clone(),
        Arc::new(ExecutorEngine(engine.clone())),
        host.clone() as Arc<dyn SuperagentHost>,
        config.clone(),
        clock.clock(),
    )
    .await?;
    engine.set_hooks(Arc::new(ExecutorHooks {
        executor: executor.clone(),
        store: store.clone(),
    }));
    let report = recovery::reconcile(&store, &executor, &board).await?;
    effects.idle(None).await;
    Ok(World {
        store,
        board,
        clock,
        seconds,
        host,
        config,
        effects,
        engine,
        executor,
        report,
    })
}

async fn first_boot() -> anyhow::Result<(TempDir, World)> {
    let (dir, store) = open_store().await?;
    let world = boot(
        store,
        FakeClock::at(T0),
        Arc::new(AtomicI64::new(1)),
        Arc::new(FakeHost::default()),
        config(&dir),
    )
    .await?;
    Ok((dir, world))
}

/// The service restarts: the old dispatcher stops, and a new engine and executor read the store.
async fn restart(old: World) -> anyhow::Result<World> {
    old.effects.drain().await;
    old.tick(1);
    boot(
        old.store.clone(),
        old.clock.clone(),
        Arc::clone(&old.seconds),
        Arc::clone(&old.host),
        old.config.clone(),
    )
    .await
}

impl World {
    fn tick(&self, seconds: i64) {
        self.seconds.fetch_add(seconds, Ordering::SeqCst);
    }

    fn now(&self) -> String {
        iso(self.seconds.load(Ordering::SeqCst))
    }

    fn trigger(&self, id: &str, thread: &str, text: &str) -> Trigger {
        Trigger {
            id: id.to_owned(),
            source: "codext".into(),
            threads: vec![ThreadRef {
                source: "codext".into(),
                id: thread.to_owned(),
            }],
            actor: ActorRef {
                source: "codext".into(),
                id: "user".into(),
            },
            human: true,
            addressed: Some(Addressed::Direct),
            at: self.now(),
            text: text.to_owned(),
            channel: Some(ChannelRef {
                source: "codext".into(),
                thread: "pm-thread".into(),
                address: Default::default(),
            }),
            url: None,
            meta: serde_json::Map::new(),
        }
    }

    /// A request of the user on `thread` that the PM accepts; answers the ticket id once the
    /// executor has done what the acceptance queued.
    async fn accept(&self, id: &str, thread: &str, text: &str) -> anyhow::Result<String> {
        self.tick(1);
        self.engine.receive(self.trigger(id, thread, text)).await?;
        self.engine.evaluate_due().await?;
        self.tick(1);
        let decision = self
            .engine
            .decide(
                id,
                Outcome::Accept(AcceptOutcome::new("Fix", "small", "requested")),
                None,
            )
            .await?;
        let ticket_id = decision
            .ticket_id
            .ok_or_else(|| anyhow::anyhow!("no ticket"))?;
        self.effects.idle(None).await;
        Ok(ticket_id)
    }

    /// The user's later message on the same thread, which the engine hands to the ticket.
    async fn follow_up(&self, id: &str, thread: &str, text: &str) -> anyhow::Result<()> {
        self.tick(1);
        self.engine.receive(self.trigger(id, thread, text)).await?;
        self.engine.evaluate_due().await?;
        self.effects.idle(None).await;
        Ok(())
    }

    async fn thread_of(&self, ticket_id: &str) -> anyhow::Result<String> {
        Ok(self
            .executor
            .session(ticket_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no session"))?
            .thread_id)
    }

    /// The worker's turn runs and ends without a report: the delivery is over.
    async fn settle_turn(&self, thread: &str) -> anyhow::Result<()> {
        self.tick(1);
        self.executor.turn_started(thread, "turn-1").await?;
        self.tick(1);
        self.executor.turn_ended(thread).await?;
        self.effects.idle(None).await;
        Ok(())
    }

    async fn ticket(&self, id: &str) -> anyhow::Result<Ticket> {
        self.store
            .ticket(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no ticket {id}"))
    }

    /// The ticket's name as the executor shows it: its execution name, else its id.
    async fn name(&self, id: &str) -> anyhow::Result<String> {
        let ticket = self.ticket(id).await?;
        Ok(ticket
            .execution
            .as_ref()
            .map(|e| e.name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or(ticket.id))
    }

    /// Retries the ticket and waits for the stop of the replaced assignment to run through.
    async fn retry(&self, ticket_id: &str) -> anyhow::Result<()> {
        let stops = self.host.count("clean_background:");
        self.tick(1);
        self.engine.retry_ticket(ticket_id).await?;
        wait_until("the retry stopped the old work", || {
            self.host.count("clean_background:") == stops + 1
        })
        .await;
        // Room for the stop to go on with what it must not do.
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.effects.idle(None).await;
        Ok(())
    }
}

fn recovery_reports(ticket: &Ticket) -> usize {
    ticket
        .assignments
        .iter()
        .filter(|a| {
            a.report.as_ref().is_some_and(|r| {
                r.state == ReportState::Blocked && r.summary.starts_with(RECOVERY_NOTE)
            })
        })
        .count()
}

fn sorted(mut items: Vec<String>) -> Vec<String> {
    items.sort();
    items
}

#[tokio::test]
async fn a_restart_continues_the_same_thread_and_sends_nothing_twice() -> anyhow::Result<()> {
    let (_dir, w) = first_boot().await?;
    let ticket = w
        .accept("1", "pm/r1", "账号切换后队列重复投递，修一下")
        .await?;
    let thread = w.thread_of(&ticket).await?;
    assert_eq!(
        (w.host.count("start_thread:"), w.host.count("deliver:")),
        (1, 1)
    );
    w.settle_turn(&thread).await?;
    assert_eq!(
        latest_report(&w.ticket(&ticket).await?).map(|r| r.state),
        Some(ReportState::Waiting)
    );

    let w = restart(w).await?;
    assert_eq!(
        w.report,
        recovery::Report {
            untouched: 1,
            ..Default::default()
        }
    );
    assert_eq!(
        (w.host.count("start_thread:"), w.host.count("deliver:")),
        (1, 1),
        "nothing went out again"
    );
    let health = w
        .board
        .get("recovery")
        .ok_or_else(|| anyhow::anyhow!("no recovery health"))?;
    assert_eq!(health.state, HealthState::Ready);
    assert!(
        health.detail.starts_with("对账：1 张在途工单"),
        "{}",
        health.detail
    );
    assert_eq!(w.executor.ticket_of_thread(&thread), Some(ticket.clone()));

    // The user's later message reaches the same worker thread, resumed first.
    w.follow_up("2", "pm/r1", "也看看重启 daemon 的情况")
        .await?;
    let deliveries = w.host.with("deliver:");
    assert_eq!(deliveries.len(), 2, "{deliveries:?}");
    assert!(
        deliveries[1].starts_with(&format!("deliver:{thread}:")),
        "{deliveries:?}"
    );
    assert_eq!(w.host.count(&format!("resume:{thread}")), 2);
    assert_eq!(w.host.count("start_thread:"), 1, "the same thread");
    assert_eq!(w.thread_of(&ticket).await?, thread);
    Ok(())
}

#[tokio::test]
async fn an_in_flight_delivery_blocks_the_ticket_keeps_its_row_and_a_retry_clears_it()
-> anyhow::Result<()> {
    let (_dir, w) = first_boot().await?;
    let ticket = w.accept("1", "pm/r1", "修一下").await?;
    let thread = w.thread_of(&ticket).await?;
    // The worker took the first message into a turn that never ended: the service restarts.
    w.tick(1);
    w.executor.turn_started(&thread, "turn-1").await?;
    assert!(w.executor.inflight_for(&thread).await?.is_some());

    let w = restart(w).await?;
    let name = w.name(&ticket).await?;
    assert_eq!(
        w.report,
        recovery::Report {
            blocked: vec![name],
            ..Default::default()
        }
    );
    let t = w.ticket(&ticket).await?;
    let report = latest_report(&t).ok_or_else(|| anyhow::anyhow!("no report"))?;
    assert_eq!(report.state, ReportState::Blocked);
    assert!(
        report.summary.starts_with(RECOVERY_NOTE),
        "{}",
        report.summary
    );
    assert!(
        w.executor.inflight_for(&thread).await?.is_some(),
        "the row stays for the person to inspect"
    );
    assert_eq!(w.host.count("deliver:"), 1, "nothing was replayed");

    // Told once: another pass changes nothing.
    let again = recovery::reconcile(&w.store, &w.executor, &HealthBoard::new()).await?;
    assert_eq!(
        again,
        recovery::Report {
            untouched: 1,
            ..Default::default()
        }
    );
    assert_eq!(recovery_reports(&w.ticket(&ticket).await?), 1);

    // The person inspected and retried: a new assignment on the same thread, the row gone.
    w.retry(&ticket).await?;
    let t = w.ticket(&ticket).await?;
    assert_eq!(t.assignments.len(), 2);
    assert_eq!(
        latest_assignment(&t).map(|a| a.state),
        Some(AssignmentState::Running)
    );
    assert!(w.executor.inflight_for(&thread).await?.is_none());
    assert_eq!(w.host.count("interrupt:"), 1, "the old turn was ended");
    assert_eq!(w.host.count("start_thread:"), 1, "the same thread");
    assert_eq!(w.thread_of(&ticket).await?, thread);
    assert_eq!(
        w.host.count("deliver:"),
        1,
        "nothing of the replaced assignment went out again"
    );
    Ok(())
}

#[tokio::test]
async fn pending_launches_queue_again_oldest_first_and_one_that_waits_for_a_person_stays()
-> anyhow::Result<()> {
    let (_dir, w) = first_boot().await?;
    w.executor.set_paused(true);
    let first = w.accept("1", "pm/r1", "第一件").await?;
    let second = w.accept("2", "pm/r2", "第二件").await?;
    let third = w.accept("3", "pm/r3", "第三件").await?;
    assert_eq!(w.host.count("start_thread:"), 0, "paused: nothing launched");
    // The third's launch had failed: its ticket waits for the person, and its launch is spent.
    w.tick(1);
    let at = w.now();
    let effects = w.effects.clone();
    w.store
        .atomic(async |tx| {
            let Some(mut t) = tx.ticket(&third).await? else {
                anyhow::bail!("no ticket");
            };
            let Some(a) = t.assignments.last_mut() else {
                anyhow::bail!("no assignment");
            };
            a.report = Some(TicketReport {
                state: ReportState::Failed,
                summary: "起不了 worker 线程".into(),
                at: at.clone(),
            });
            let key = a.id.clone();
            tx.save_ticket(&t).await?;
            effects.cancel(tx, KIND_LAUNCH, &key).await
        })
        .await?;

    let w = restart(w).await?;
    let names = (
        w.name(&first).await?,
        w.name(&second).await?,
        w.name(&third).await?,
    );
    assert_eq!(
        w.report,
        recovery::Report {
            relaunched: vec![names.0.clone(), names.1.clone()],
            waiting: vec![names.2.clone()],
            ..Default::default()
        }
    );
    let started: Vec<String> = w
        .host
        .with("start_thread:")
        .into_iter()
        .map(|c| c.rsplit('/').next().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(sorted(started), sorted(vec![names.0, names.1]));
    assert_eq!(
        latest_assignment(&w.ticket(&third).await?).map(|a| a.state),
        Some(AssignmentState::Pending),
        "left for the person's retry"
    );
    Ok(())
}

#[tokio::test]
async fn a_retry_keeps_the_thread_its_budget_and_its_tools_and_hands_back_the_untaken_inputs()
-> anyhow::Result<()> {
    let (dir, w) = first_boot().await?;
    let ticket = w.accept("1", "pm/r1", "修一下").await?;
    let thread = w.thread_of(&ticket).await?;
    w.settle_turn(&thread).await?;
    // A later message is delivered, but the worker takes no turn with it before it fails.
    w.follow_up("2", "pm/r1", "顺便加个回归测试").await?;
    assert_eq!(w.host.count("deliver:"), 2);
    w.tick(1);
    w.executor
        .report_from_worker(
            &thread,
            WorkerReport {
                state: ReportState::Failed,
                summary: "编译失败，停了".into(),
                ..WorkerReport::default()
            },
        )
        .await
        .map_err(|err| anyhow::anyhow!(err))?;
    assert_eq!(
        latest_report(&w.ticket(&ticket).await?).map(|r| r.state),
        Some(ReportState::Failed)
    );
    let interrupts = w.host.count("interrupt:");

    w.retry(&ticket).await?;
    let t = w.ticket(&ticket).await?;
    assert_eq!(t.assignments.len(), 2);
    assert_eq!(
        latest_assignment(&t).map(|a| a.state),
        Some(AssignmentState::Running)
    );
    assert_eq!(
        w.host.count("interrupt:"),
        interrupts + 1,
        "the old turn was ended"
    );
    assert_eq!(
        w.host.count("start_thread:"),
        1,
        "the same thread and worktree"
    );
    assert_eq!(w.thread_of(&ticket).await?, thread);
    assert_eq!(w.host.count("goal_clear:"), 0, "the budget stays");
    let ended = std::fs::read_to_string(dir.path().join("state/ended-threads")).unwrap_or_default();
    assert!(!ended.contains(&thread), "the thread keeps its tools");
    // The untaken message goes out again to the new assignment, and nothing else.
    let deliveries = w.host.with("deliver:");
    assert_eq!(deliveries.len(), 3, "{deliveries:?}");
    assert!(
        deliveries[2].starts_with(&format!("deliver:{thread}:")),
        "{deliveries:?}"
    );
    assert_eq!(
        deliveries[2].rsplit(':').next(),
        deliveries[1].rsplit(':').next(),
        "the same message as before: {deliveries:?}"
    );
    assert!(w.executor.inflight_for(&thread).await?.is_some());
    Ok(())
}
