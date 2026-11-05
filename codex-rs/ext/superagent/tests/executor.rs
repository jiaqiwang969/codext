//! The codext-thread executor against a fake host and a fake engine: the cases of the prototype's
//! `tests/codex.spec.ts`, the slot rules of `runtime-concurrency.spec.ts` and the turn-end rules of
//! `background.spec.ts`, plus the T-14 acceptance of design §10.1.

mod common;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_protocol::ThreadId;
use codex_superagent_extension::core::ChannelRef;
use codex_superagent_extension::core::Handback;
use codex_superagent_extension::core::TriggerDisposition;
use codex_superagent_extension::BoxFut;
use codex_superagent_extension::Effects;
use codex_superagent_extension::HealthBoard;
use codex_superagent_extension::Store;
use codex_superagent_extension::Tx;
use codex_superagent_extension::core::Assignment;
use codex_superagent_extension::core::AssignmentState;
use codex_superagent_extension::core::Ended;
use codex_superagent_extension::core::Execution;
use codex_superagent_extension::core::Report;
use codex_superagent_extension::core::ReportState;
use codex_superagent_extension::core::Ticket;
use codex_superagent_extension::core::TicketEndBy;
use codex_superagent_extension::core::TicketInput;
use codex_superagent_extension::core::TicketOutcome;
use codex_superagent_extension::core::latest_assignment;
use codex_superagent_extension::core::worker_thread_key;
use codex_superagent_extension::executor::CodextThreadExecutor;
use codex_superagent_extension::executor::EngineForExecutor;
use codex_superagent_extension::executor::Executor;
use codex_superagent_extension::executor::ExecutorConfig;
use codex_superagent_extension::executor::WorkerReport;
use codex_superagent_extension::executor::codext_thread::RECOVERY_NOTE;
use codex_superagent_extension::executor::codext_thread::SessionRecord;
use codex_superagent_extension::executor::codext_thread::session_key;
use codex_superagent_extension::executor::install_executor;
use codex_superagent_extension::host::DeliveryMode;
use codex_superagent_extension::host::GoalStatus;
use codex_superagent_extension::host::SuperagentHost;
use codex_superagent_extension::host::ThreadSpec;
use common::FakeClock;
use common::T0;
use common::open_store;
use common::trigger;
use common::wait_until;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[derive(Default)]
struct FakeHost {
    calls: Mutex<Vec<String>>,
    children: Mutex<Vec<ThreadId>>,
    background: Mutex<Vec<String>>,
    goal: Mutex<Option<GoalStatus>>,
    /// codext's per-thread token count, as the host would report it.
    tokens: Mutex<Option<i64>>,
    /// Every delivered text in full (the calls keep only the first line).
    texts: Mutex<Vec<String>>,
    fail_deliver: AtomicBool,
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

    fn count(&self, prefix: &str) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }
}

impl SuperagentHost for FakeHost {
    fn start_thread(&self, spec: ThreadSpec) -> BoxFut<'_, anyhow::Result<ThreadId>> {
        self.push(format!(
            "start_thread:{}:{}",
            spec.profile.unwrap_or_default(),
            spec.cwd.display()
        ));
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
        self.texts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(text);
        let fail = self.fail_deliver.load(Ordering::SeqCst);
        Box::pin(async move {
            if fail {
                anyhow::bail!("thread gone")
            }
            Ok(())
        })
    }

    fn interrupt(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        self.push(format!("interrupt:{thread}"));
        Box::pin(async { Ok(()) })
    }

    fn clean_background(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        self.push(format!("clean_background:{thread}"));
        self.background
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        Box::pin(async { Ok(()) })
    }

    fn background_commands(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<String>>> {
        let running = self
            .background
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Box::pin(async move { Ok(running) })
    }

    fn children(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<ThreadId>>> {
        let children = self
            .children
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Box::pin(async move { Ok(children) })
    }

    fn goal_set(
        &self,
        thread: ThreadId,
        _objective: String,
        budget: Option<i64>,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        self.push(format!("goal_set:{thread}:{}", budget.unwrap_or_default()));
        Box::pin(async { Ok(()) })
    }

    fn goal_clear(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        self.push(format!("goal_clear:{thread}"));
        Box::pin(async { Ok(()) })
    }

    fn tokens_used(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Option<i64>>> {
        let tokens = *self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
        Box::pin(async move { Ok(tokens) })
    }

    fn goal_status(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Option<GoalStatus>>> {
        let goal = self
            .goal
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Box::pin(async move { Ok(goal) })
    }
}

/// Records the callbacks and applies the smallest state change the executor expects of the engine.
#[derive(Default)]
struct FakeEngine {
    calls: Mutex<Vec<String>>,
    fail_inputs_delivered: AtomicBool,
}

impl FakeEngine {
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

    fn has(&self, prefix: &str) -> bool {
        self.calls().iter().any(|c| c.starts_with(prefix))
    }
}

async fn update_ticket(
    tx: &mut Tx,
    ticket_id: &str,
    change: impl FnOnce(&mut Ticket),
) -> anyhow::Result<()> {
    if let Some(mut ticket) = tx.ticket(ticket_id).await? {
        change(&mut ticket);
        tx.save_ticket(&ticket).await?;
    }
    Ok(())
}

impl EngineForExecutor for FakeEngine {
    fn note<'a>(
        &'a self,
        _tx: &'a mut Tx,
        ticket_id: &'a str,
        text: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        self.push(format!("note:{ticket_id}:{text}"));
        Box::pin(async { Ok(()) })
    }

    fn assignment_started<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        assignment_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        self.push(format!("assignment_started:{ticket_id}:{assignment_id}"));
        Box::pin(update_ticket(tx, ticket_id, move |ticket| {
            if let Some(assignment) = ticket
                .assignments
                .iter_mut()
                .find(|a| a.id == assignment_id)
            {
                assignment.state = AssignmentState::Running;
            }
        }))
    }

    fn inputs_delivered<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        assignment_id: &'a str,
        input_ids: &'a [String],
    ) -> BoxFut<'a, anyhow::Result<()>> {
        self.push(format!(
            "inputs_delivered:{ticket_id}:{assignment_id}:{}",
            input_ids.join(",")
        ));
        let fail = self.fail_inputs_delivered.load(Ordering::SeqCst);
        Box::pin(async move {
            if fail {
                anyhow::bail!("engine refused");
            }
            update_ticket(tx, ticket_id, move |ticket| {
                ticket.undelivered.retain(|id| !input_ids.contains(id))
            })
            .await
        })
    }

    fn turn_started<'a>(
        &'a self,
        _tx: &'a mut Tx,
        ticket_id: &'a str,
        assignment_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        self.push(format!("turn_started:{ticket_id}:{assignment_id}"));
        Box::pin(async { Ok(()) })
    }

    fn inputs_read<'a>(
        &'a self,
        _tx: &'a mut Tx,
        ticket_id: &'a str,
        input_ids: &'a [String],
    ) -> BoxFut<'a, anyhow::Result<()>> {
        self.push(format!("inputs_read:{ticket_id}:{}", input_ids.join(",")));
        Box::pin(async { Ok(()) })
    }

    fn turn_ended<'a>(
        &'a self,
        _tx: &'a mut Tx,
        ticket_id: &'a str,
        assignment_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        self.push(format!("turn_ended:{ticket_id}:{assignment_id}"));
        Box::pin(async { Ok(()) })
    }

    fn delivery_failed<'a>(
        &'a self,
        _tx: &'a mut Tx,
        ticket_id: &'a str,
        _assignment_id: &'a str,
        error: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        self.push(format!("delivery_failed:{ticket_id}:{error}"));
        Box::pin(async { Ok(()) })
    }

    fn report<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        report: WorkerReport,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        self.push(format!(
            "report:{ticket_id}:{:?}:{}",
            report.state, report.summary
        ));
        Box::pin(update_ticket(tx, ticket_id, move |ticket| {
            if let Some(assignment) = ticket.assignments.last_mut() {
                assignment.report = Some(Report {
                    state: report.state,
                    summary: report.summary,
                    at: "1".into(),
                });
            }
        }))
    }

    fn acknowledge<'a>(
        &'a self,
        _tx: &'a mut Tx,
        ticket_id: &'a str,
        trigger_id: &'a str,
        _text: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        self.push(format!("ack:{ticket_id}:{trigger_id}"));
        Box::pin(async { Ok(()) })
    }

    fn mark_unrelated<'a>(
        &'a self,
        _tx: &'a mut Tx,
        ticket_id: &'a str,
        thread_key: &'a str,
        _reason: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        self.push(format!("mark_unrelated:{ticket_id}:{thread_key}"));
        Box::pin(async { Ok(()) })
    }

    fn follow_for_context<'a>(
        &'a self,
        _tx: &'a mut Tx,
        ticket_id: &'a str,
        thread_key: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        self.push(format!("follow:{ticket_id}:{thread_key}"));
        Box::pin(async { Ok(()) })
    }

    fn handback<'a>(
        &'a self,
        _tx: &'a mut Tx,
        ticket_id: &'a str,
        trigger_id: &'a str,
        _reason: &'a str,
    ) -> BoxFut<'a, anyhow::Result<bool>> {
        self.push(format!("handback:{ticket_id}:{trigger_id}"));
        Box::pin(async { Ok(true) })
    }

    fn assignment_interrupted<'a>(
        &'a self,
        _tx: &'a mut Tx,
        ticket_id: &'a str,
        assignment_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        self.push(format!(
            "assignment_interrupted:{ticket_id}:{assignment_id}"
        ));
        Box::pin(async { Ok(()) })
    }

    fn background_ended<'a>(
        &'a self,
        _tx: &'a mut Tx,
        ticket_id: &'a str,
        text: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        self.push(format!("background_ended:{ticket_id}:{text}"));
        Box::pin(async { Ok(()) })
    }
}

struct Setup {
    _dir: TempDir,
    store: Store,
    clock: FakeClock,
    host: Arc<FakeHost>,
    engine: Arc<FakeEngine>,
    effects: Effects,
    executor: CodextThreadExecutor,
    config: ExecutorConfig,
}

fn config(dir: &TempDir, max_workers: usize) -> ExecutorConfig {
    ExecutorConfig {
        repo: dir.path().join("repo"),
        worktree_root: dir.path().join("wt"),
        base_branch: "superagent/integration".into(),
        create_worktrees: false,
        profiles: vec!["A".into(), "B".into()],
        max_per_profile: 3,
        max_workers,
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

async fn setup_with(max_workers: usize) -> anyhow::Result<Setup> {
    let (dir, store) = open_store().await?;
    let clock = FakeClock::at(T0);
    let host = Arc::new(FakeHost::default());
    let engine = Arc::new(FakeEngine::default());
    let effects = Effects::new(store.clone(), HealthBoard::new(), clock.clock()).await?;
    effects.start();
    let config = config(&dir, max_workers);
    let executor = install_executor(
        store.clone(),
        effects.clone(),
        engine.clone() as Arc<dyn EngineForExecutor>,
        host.clone() as Arc<dyn SuperagentHost>,
        config.clone(),
        clock.clock(),
    )
    .await?;
    Ok(Setup {
        _dir: dir,
        store,
        clock,
        host,
        engine,
        effects,
        executor,
        config,
    })
}

async fn setup() -> anyhow::Result<Setup> {
    setup_with(4).await
}

/// A second executor on the same store, as after a restart of the service.
async fn restart(s: &Setup) -> anyhow::Result<(Effects, CodextThreadExecutor)> {
    let effects = Effects::new(s.store.clone(), HealthBoard::new(), s.clock.clock()).await?;
    effects.start();
    let executor = install_executor(
        s.store.clone(),
        effects.clone(),
        s.engine.clone() as Arc<dyn EngineForExecutor>,
        s.host.clone() as Arc<dyn SuperagentHost>,
        s.config.clone(),
        s.clock.clock(),
    )
    .await?;
    executor.restore().await?;
    Ok((effects, executor))
}

fn ticket(id: &str) -> Ticket {
    let mut ticket = common::ticket(id, false, &[AssignmentState::Pending], &[]);
    ticket.title = format!("fix {id}");
    ticket.execution = Some(Execution {
        name: id.to_owned(),
        directory: String::new(),
        assignment: "修好重复投递".into(),
    });
    ticket.origin.trigger_id = "tr1".into();
    ticket
}

async fn save(s: &Setup, ticket: &Ticket) -> anyhow::Result<()> {
    s.store
        .atomic(async |tx| {
            tx.save_trigger(&trigger(
                "tr1",
                "codext",
                &[("codext", "pm/TR-1")],
                "1",
                "账号切换后队列重复投递，修一下",
            ))
            .await?;
            tx.save_ticket(ticket).await
        })
        .await
}

/// Saves the ticket, asks the executor to start its latest assignment and waits for the launch.
async fn launch(
    s: &Setup,
    executor: &CodextThreadExecutor,
    effects: &Effects,
    ticket: &Ticket,
) -> anyhow::Result<()> {
    save(s, ticket).await?;
    let assignment = latest_assignment(ticket)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no assignment"))?;
    s.store
        .atomic(async |tx| executor.start(tx, ticket, &assignment).await)
        .await?;
    effects.idle(None).await;
    Ok(())
}

async fn thread_of(s: &Setup, ticket_id: &str) -> anyhow::Result<String> {
    Ok(s.executor
        .session(ticket_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no session"))?
        .thread_id)
}

#[tokio::test]
async fn launches_a_worker_thread_delivers_the_assignment_and_reads_the_turn() -> anyhow::Result<()>
{
    let s = setup().await?;
    let t = ticket("t017");
    launch(&s, &s.executor, &s.effects, &t).await?;
    let calls = s.host.calls();
    assert_eq!(s.host.count("start_thread:A:"), 1);
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("bind_profile:") && c.ends_with(":A")),
        "{calls:?}"
    );
    // The budget is the executor's own count; a Goal would make the thread continue on its own.
    assert!(
        !calls.iter().any(|c| c.starts_with("goal_set:")),
        "{calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("deliver:") && c.contains(":Queue:工单 t017：fix t017")),
        "{calls:?}"
    );
    assert!(s.engine.has("assignment_started:t017:t017-0"));
    assert!(s.engine.has("inputs_delivered:t017:t017-0:"));
    let thread = thread_of(&s, "t017").await?;
    assert_eq!(
        s.executor.ticket_of_thread(&thread),
        Some("t017".to_owned())
    );
    // Intake places a person's input in the worker thread through this index (decision #9).
    assert_eq!(
        s.store
            .kv_get::<String>(&worker_thread_key(&thread))
            .await?,
        Some("t017".to_owned())
    );
    assert!(
        s.executor.inflight_for(&thread).await?.is_some(),
        "inflight written before the delivery"
    );
    assert!(s.executor.slots().holds("t017"));
    // The turn runs: its inputs are read; it ends without a report: waiting, slot freed.
    s.executor.turn_started(&thread, "turn-1").await?;
    assert!(s.engine.has("inputs_read:t017:"));
    assert_eq!(
        s.executor
            .inflight_for(&thread)
            .await?
            .and_then(|i| i.turn_id),
        Some("turn-1".to_owned())
    );
    s.executor.turn_ended(&thread).await?;
    assert!(s.executor.inflight_for(&thread).await?.is_none());
    assert!(s.engine.has("turn_ended:t017:t017-0"));
    assert!(s.engine.has("report:t017:Waiting:"));
    assert!(!s.executor.slots().holds("t017"));
    Ok(())
}

#[tokio::test]
async fn continues_the_same_thread_after_a_restart() -> anyhow::Result<()> {
    let s = setup().await?;
    let mut t = ticket("t017");
    launch(&s, &s.executor, &s.effects, &t).await?;
    let thread = thread_of(&s, "t017").await?;
    // A retry: the first assignment ended, a second one is pending.
    t.assignments[0].state = AssignmentState::Ended;
    t.assignments.push(Assignment::new("t017-1", "2"));
    let (effects, executor) = restart(&s).await?;
    launch(&s, &executor, &effects, &t).await?;
    assert_eq!(
        s.host.count("start_thread:"),
        1,
        "the session continues on the same thread"
    );
    assert_eq!(executor.ticket_of_thread(&thread), Some("t017".to_owned()));
    assert!(s.engine.has("assignment_started:t017:t017-1"));
    Ok(())
}

#[tokio::test]
async fn restore_backfills_the_worker_thread_index_of_an_older_ledger() -> anyhow::Result<()> {
    let s = setup().await?;
    let t = ticket("t017");
    launch(&s, &s.executor, &s.effects, &t).await?;
    let thread = thread_of(&s, "t017").await?;
    // A ledger written before the index existed has the session but not the index.
    s.store
        .atomic(async |tx| tx.kv_delete(&worker_thread_key(&thread)).await)
        .await?;
    restart(&s).await?;
    assert_eq!(
        s.store
            .kv_get::<String>(&worker_thread_key(&thread))
            .await?,
        Some("t017".to_owned())
    );
    Ok(())
}

#[tokio::test]
async fn refuses_a_session_bound_to_another_account() -> anyhow::Result<()> {
    let s = setup().await?;
    let t = ticket("t017");
    let record = SessionRecord {
        account: "Z".into(),
        thread_id: ThreadId::new().to_string(),
        cwd: "/x".into(),
        model: None,
        turns: 1,
        tokens_at_start: 0,
    };
    s.store
        .atomic(async |tx| tx.kv_set(&session_key("t017"), &record).await)
        .await?;
    launch(&s, &s.executor, &s.effects, &t).await?;
    assert_eq!(s.host.count("start_thread:"), 0);
    assert!(
        s.engine
            .calls()
            .iter()
            .any(|c| c.starts_with("report:t017:Failed:Account/workspace differs")),
        "{:?}",
        s.engine.calls()
    );
    Ok(())
}

#[tokio::test]
async fn never_replays_a_delivery_that_was_in_flight() -> anyhow::Result<()> {
    let s = setup().await?;
    let t = ticket("t017");
    launch(&s, &s.executor, &s.effects, &t).await?;
    let thread = thread_of(&s, "t017").await?;
    assert!(s.executor.inflight_for(&thread).await?.is_some());
    // The service restarts while the delivery is in flight.
    let (_effects, executor) = restart(&s).await?;
    assert!(
        executor.inflight_for(&thread).await?.is_some(),
        "the row stays for the person to inspect"
    );
    let calls = s.engine.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("report:t017:Blocked:") && c.contains(RECOVERY_NOTE)),
        "{calls:?}"
    );
    assert_eq!(s.host.count("deliver:"), 1, "nothing was sent again");
    Ok(())
}

#[tokio::test]
async fn pins_the_account_and_spreads_new_workers() -> anyhow::Result<()> {
    let s = setup().await?;
    launch(&s, &s.executor, &s.effects, &ticket("t017")).await?;
    launch(&s, &s.executor, &s.effects, &ticket("t021")).await?;
    assert_eq!(s.host.count("start_thread:A:"), 1);
    assert_eq!(s.host.count("start_thread:B:"), 1);
    assert_eq!(
        s.executor.session("t017").await?.map(|r| r.account),
        Some("A".to_owned())
    );
    assert_eq!(
        s.executor.session("t021").await?.map(|r| r.account),
        Some("B".to_owned())
    );
    Ok(())
}

#[tokio::test]
async fn downgrades_ready_to_progress_while_inputs_wait() -> anyhow::Result<()> {
    let s = setup().await?;
    let t = ticket("t017");
    launch(&s, &s.executor, &s.effects, &t).await?;
    let thread = thread_of(&s, "t017").await?;
    s.executor.turn_started(&thread, "turn-1").await?;
    s.store
        .atomic(async |tx| {
            update_ticket(tx, "t017", |ticket| {
                ticket.inputs.push(TicketInput::Note {
                    id: "i2".into(),
                    text: "也看看重启".into(),
                    at: "3".into(),
                });
                ticket.undelivered.push("i2".into());
            })
            .await
        })
        .await?;
    let report = WorkerReport {
        state: ReportState::Ready,
        summary: "修好了".into(),
        evidence: vec!["t017/test.log".into()],
        ..WorkerReport::default()
    };
    s.executor
        .report_from_worker(&thread, report)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    assert!(
        s.engine.has("report:t017:Progress:修好了"),
        "{:?}",
        s.engine.calls()
    );
    Ok(())
}

#[tokio::test]
async fn checks_a_report_before_recording_it() -> anyhow::Result<()> {
    let s = setup().await?;
    let t = ticket("t017");
    launch(&s, &s.executor, &s.effects, &t).await?;
    let thread = thread_of(&s, "t017").await?;
    let base = WorkerReport {
        state: ReportState::Ready,
        summary: "ok".into(),
        evidence: vec!["e".into()],
        ..WorkerReport::default()
    };
    let long = WorkerReport {
        summary: "字".repeat(601),
        ..base.clone()
    };
    assert!(CodextThreadExecutor::validate_report(&long).is_err());
    assert!(
        CodextThreadExecutor::validate_report(&WorkerReport {
            evidence: vec![],
            ..base.clone()
        })
        .is_err()
    );
    assert!(
        CodextThreadExecutor::validate_report(&WorkerReport {
            question: Some("哪个分支？".into()),
            reply_to: Some("tr1".into()),
            ..base.clone()
        })
        .is_err()
    );
    assert!(
        CodextThreadExecutor::validate_report(&WorkerReport {
            state: ReportState::Blocked,
            question: Some("哪个分支？".into()),
            reply_to: None,
            ..base.clone()
        })
        .is_err()
    );
    assert!(CodextThreadExecutor::validate_report(&base).is_ok());
    // A helper still runs: no ready report.
    s.host
        .children
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(ThreadId::new());
    let refused = s.executor.report_from_worker(&thread, base.clone()).await;
    assert!(
        refused.as_ref().is_err_and(|e| e.contains("先收掉 helper")),
        "{refused:?}"
    );
    assert!(
        s.executor
            .report_from_worker("not-a-worker", base)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn a_ready_report_frees_the_slot_and_stops_the_helpers() -> anyhow::Result<()> {
    let s = setup().await?;
    let t = ticket("t017");
    launch(&s, &s.executor, &s.effects, &t).await?;
    let thread = thread_of(&s, "t017").await?;
    s.executor.turn_started(&thread, "turn-1").await?;
    let helper = ThreadId::new();
    s.host
        .children
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(helper);
    // The worker reports progress first, then ready once the helper is done.
    let progress = WorkerReport {
        state: ReportState::Progress,
        summary: "查完了".into(),
        ..WorkerReport::default()
    };
    s.executor
        .report_from_worker(&thread, progress)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    assert!(s.executor.slots().holds("t017"));
    s.host
        .children
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    let ready = WorkerReport {
        state: ReportState::Ready,
        summary: "修好了".into(),
        evidence: vec!["t017/test.log".into()],
        ..WorkerReport::default()
    };
    s.executor
        .report_from_worker(&thread, ready)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    assert!(!s.executor.slots().holds("t017"));
    s.executor.turn_ended(&thread).await?;
    assert!(s.engine.has("report:t017:Ready:"));
    assert!(
        !s.engine.has("report:t017:Waiting:"),
        "a handoff is not followed by a waiting report"
    );
    Ok(())
}

#[tokio::test]
async fn stop_ends_the_turn_the_background_commands_the_helpers_and_the_goal() -> anyhow::Result<()>
{
    let s = setup().await?;
    let t = ticket("t017");
    launch(&s, &s.executor, &s.effects, &t).await?;
    let thread = thread_of(&s, "t017").await?;
    let helper = ThreadId::new();
    s.host
        .children
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(helper);
    let assignment = latest_assignment(&t)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no assignment"))?;
    s.store
        .atomic(async |tx| {
            // The engine ends the ticket in the same commit before it asks for the stop; an
            // assignment ended under an open ticket (a retry) keeps the thread instead.
            let Some(mut ended) = tx.ticket("t017").await? else {
                anyhow::bail!("no ticket");
            };
            ended.ended = Some(Ended {
                at: "2".into(),
                by: TicketEndBy::Human,
                outcome: TicketOutcome::Stopped,
                reason: Some("ended".into()),
            });
            tx.save_ticket(&ended).await?;
            s.executor.stop(tx, &ended, &assignment, "ended").await
        })
        .await?;
    let host = s.host.clone();
    wait_until("goal cleared", move || host.count("goal_clear:") == 1).await;
    let calls = s.host.calls();
    assert!(calls.contains(&format!("interrupt:{thread}")), "{calls:?}");
    assert!(
        calls.contains(&format!("clean_background:{thread}")),
        "{calls:?}"
    );
    assert!(calls.contains(&format!("interrupt:{helper}")), "{calls:?}");
    let listed = std::fs::read_to_string(&s.config.ended_threads_file)?;
    assert!(listed.lines().any(|line| line == thread), "{listed}");
    assert!(!s.executor.slots().holds("t017"));
    Ok(())
}

#[tokio::test]
async fn a_delivery_whose_record_fails_is_never_sent_again() -> anyhow::Result<()> {
    let s = setup().await?;
    s.engine.fail_inputs_delivered.store(true, Ordering::SeqCst);
    let t = ticket("t017");
    launch(&s, &s.executor, &s.effects, &t).await?;
    let thread = thread_of(&s, "t017").await?;
    assert_eq!(
        s.executor.session("t017").await?.map(|r| r.turns),
        Some(0),
        "the rolled-back transaction kept nothing"
    );
    assert!(
        s.executor.inflight_for(&thread).await?.is_some(),
        "the delivery went out; its row stays"
    );
    assert!(
        s.effects.pending("launch", Some("t017-0")).await?,
        "the launch waits for its retry"
    );
    assert!(!s.executor.slots().holds("t017"));
    // The retry finds the in-flight delivery: the worker may have the input, so nothing is sent
    // again; the ticket waits for a person (decision #8).
    s.engine
        .fail_inputs_delivered
        .store(false, Ordering::SeqCst);
    s.clock.advance(5_000);
    s.effects.idle(None).await;
    assert_eq!(s.host.count("deliver:"), 1);
    assert!(s.executor.inflight_for(&thread).await?.is_some());
    assert!(
        s.engine
            .calls()
            .iter()
            .any(|c| c.starts_with("report:t017:Blocked:") && c.contains(RECOVERY_NOTE)),
        "{:?}",
        s.engine.calls()
    );
    assert_eq!(s.host.count("start_thread:"), 1, "the session is kept");
    Ok(())
}

#[tokio::test]
async fn a_failed_delivery_clears_the_inflight_row_and_tells_the_engine() -> anyhow::Result<()> {
    let s = setup().await?;
    s.host.fail_deliver.store(true, Ordering::SeqCst);
    let t = ticket("t017");
    launch(&s, &s.executor, &s.effects, &t).await?;
    let thread = thread_of(&s, "t017").await?;
    assert!(s.executor.inflight_for(&thread).await?.is_none());
    assert!(
        s.engine.has("delivery_failed:t017:thread gone"),
        "{:?}",
        s.engine.calls()
    );
    Ok(())
}

#[tokio::test]
async fn keeps_the_slot_while_background_commands_run_then_ends_them_at_the_limit()
-> anyhow::Result<()> {
    let s = setup().await?;
    let t = ticket("t017");
    launch(&s, &s.executor, &s.effects, &t).await?;
    let thread = thread_of(&s, "t017").await?;
    s.executor.turn_started(&thread, "turn-1").await?;
    s.host
        .background
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push("cargo test".into());
    s.executor.turn_ended(&thread).await?;
    assert!(
        s.executor.slots().holds("t017"),
        "the slot is kept while commands run"
    );
    assert!(!s.engine.has("report:t017:Waiting:"));
    let executor = s.executor.clone();
    wait_until("slot freed at the limit", move || {
        !executor.slots().holds("t017")
    })
    .await;
    assert_eq!(s.host.count("clean_background:"), 1);
    assert!(
        s.engine
            .calls()
            .iter()
            .any(|c| c.starts_with("report:t017:Waiting:后台命令超过时限")),
        "{:?}",
        s.engine.calls()
    );
    Ok(())
}

#[tokio::test]
async fn an_assignment_over_its_token_budget_is_blocked() -> anyhow::Result<()> {
    let s = setup().await?;
    let t = ticket("t017");
    *s.host.tokens.lock().unwrap_or_else(PoisonError::into_inner) = Some(1_000);
    launch(&s, &s.executor, &s.effects, &t).await?;
    let thread = thread_of(&s, "t017").await?;
    s.executor.turn_started(&thread, "turn-1").await?;
    // 1_000 at the start, 40_000 used: under the 50_000 budget, the turn ends like any other.
    *s.host.tokens.lock().unwrap_or_else(PoisonError::into_inner) = Some(41_000);
    s.executor.turn_ended(&thread).await?;
    assert!(
        !s.engine
            .calls()
            .iter()
            .any(|c| c.starts_with("report:t017:Blocked")),
        "{:?}",
        s.engine.calls()
    );
    s.executor.turn_started(&thread, "turn-2").await?;
    *s.host.tokens.lock().unwrap_or_else(PoisonError::into_inner) = Some(52_000);
    s.executor.turn_ended(&thread).await?;
    assert!(
        s.engine
            .calls()
            .iter()
            .any(|c| c.starts_with("report:t017:Blocked:预算用完：这次指派已用 51000 token，预算 50000")),
        "{:?}",
        s.engine.calls()
    );
    assert!(!s.executor.slots().holds("t017"));
    Ok(())
}

#[tokio::test]
async fn an_exhausted_budget_blocks_the_ticket() -> anyhow::Result<()> {
    let s = setup().await?;
    let t = ticket("t017");
    launch(&s, &s.executor, &s.effects, &t).await?;
    let thread = thread_of(&s, "t017").await?;
    s.executor.turn_started(&thread, "turn-1").await?;
    *s.host.goal.lock().unwrap_or_else(PoisonError::into_inner) = Some(GoalStatus::BudgetLimited);
    s.executor.turn_ended(&thread).await?;
    assert!(
        s.engine
            .calls()
            .iter()
            .any(|c| c.starts_with("report:t017:Blocked:预算用完")),
        "{:?}",
        s.engine.calls()
    );
    assert!(!s.executor.slots().holds("t017"));
    Ok(())
}

#[tokio::test]
async fn full_slots_queue_a_launch_until_one_frees() -> anyhow::Result<()> {
    let s = setup_with(1).await?;
    launch(&s, &s.executor, &s.effects, &ticket("t017")).await?;
    launch(&s, &s.executor, &s.effects, &ticket("t021")).await?;
    assert_eq!(s.host.count("start_thread:"), 1);
    assert!(
        s.effects.pending("launch", Some("t021-0")).await?,
        "the second launch waits, without an error"
    );
    let thread = thread_of(&s, "t017").await?;
    s.executor.turn_started(&thread, "turn-1").await?;
    s.executor.turn_ended(&thread).await?;
    s.clock.advance(5_000);
    s.effects.idle(None).await;
    assert_eq!(s.host.count("start_thread:"), 2);
    Ok(())
}

#[tokio::test]
async fn a_persons_message_reaches_an_idle_worker() -> anyhow::Result<()> {
    let s = setup().await?;
    let t = ticket("t017");
    launch(&s, &s.executor, &s.effects, &t).await?;
    let thread = thread_of(&s, "t017").await?;
    s.executor.turn_started(&thread, "turn-1").await?;
    s.executor.turn_ended(&thread).await?;
    let mut ticket = s
        .store
        .ticket("t017")
        .await?
        .ok_or_else(|| anyhow::anyhow!("t017"))?;
    ticket.inputs.push(TicketInput::Trigger {
        id: "i2".into(),
        trigger_id: "tr1".into(),
        at: "4".into(),
        note: None,
        human: Some(true),
        context: None,
        read_at: None,
        delivered_to: None,
        delivered_at: None,
    });
    ticket.undelivered.push("i2".into());
    let assignment = latest_assignment(&ticket)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no assignment"))?;
    s.store
        .atomic(async |tx| {
            tx.save_ticket(&ticket).await?;
            s.executor.deliver(tx, &ticket, &assignment).await
        })
        .await?;
    s.effects.idle(None).await;
    assert_eq!(s.host.count("deliver:"), 2);
    assert!(
        s.host
            .calls()
            .last()
            .is_some_and(|c| c.contains(":Queue:[输入 i2]")),
        "{:?}",
        s.host.calls()
    );
    assert!(
        s.executor.slots().holds("t017"),
        "the wake holds a slot until the turn ends"
    );
    Ok(())
}

#[tokio::test]
async fn relays_a_conclusion_to_the_pm_once_and_never_plain_progress() -> anyhow::Result<()> {
    let s = setup().await?;
    let t = ticket("t017");
    launch(&s, &s.executor, &s.effects, &t).await?;
    let thread = thread_of(&s, "t017").await?;
    s.executor.turn_started(&thread, "turn-1").await?;
    let progress = WorkerReport {
        state: ReportState::Progress,
        summary: "改了一半".into(),
        ..WorkerReport::default()
    };
    s.executor
        .report_from_worker(&thread, progress)
        .await
        .map_err(|err| anyhow::anyhow!(err))?;
    assert!(
        s.effects.queued("relay").await?.is_empty(),
        "progress stays on the ticket"
    );
    let ready = WorkerReport {
        state: ReportState::Ready,
        summary: "修好了：切换账号后不再重复投递".into(),
        evidence: vec!["evidence/t017/tests.md".into()],
        ..WorkerReport::default()
    };
    s.executor
        .report_from_worker(&thread, ready.clone())
        .await
        .map_err(|err| anyhow::anyhow!(err))?;
    let relays = s.effects.queued("relay").await?;
    assert_eq!(relays.len(), 1, "{relays:?}");
    assert!(relays[0].0.starts_with("report:"), "{}", relays[0].0);
    assert!(
        relays[0].1["text"]
            .as_str()
            .unwrap_or_default()
            .contains("worker_report"),
        "{}",
        relays[0].1
    );
    // The same conclusion again lands on the same relay: the PM hears it once.
    s.executor
        .report_from_worker(&thread, ready)
        .await
        .map_err(|err| anyhow::anyhow!(err))?;
    assert_eq!(s.effects.queued("relay").await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_background_command_that_ends_wakes_the_worker() -> anyhow::Result<()> {
    let s = setup().await?;
    let t = ticket("t017");
    launch(&s, &s.executor, &s.effects, &t).await?;
    let thread = thread_of(&s, "t017").await?;
    s.executor.turn_started(&thread, "turn-1").await?;
    s.host
        .background
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push("cargo test -p codex-app-server".into());
    s.executor.turn_ended(&thread).await?;
    assert!(s.executor.slots().holds("t017"));
    // The command ends well before the limit.
    s.host
        .background
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    let engine = s.engine.clone();
    wait_until("background end recorded", move || {
        engine.has("background_ended:t017:后台命令结束：cargo test -p codex-app-server")
    })
    .await;
    let executor = s.executor.clone();
    wait_until("slot freed", move || !executor.slots().holds("t017")).await;
    assert_eq!(s.host.count("clean_background:"), 0, "nothing was killed");
    assert!(!s.engine.has("report:t017:Waiting:"));
    Ok(())
}

#[tokio::test]
async fn steers_a_persons_message_into_a_running_turn() -> anyhow::Result<()> {
    let s = setup().await?;
    let t = ticket("t017");
    launch(&s, &s.executor, &s.effects, &t).await?;
    let thread = thread_of(&s, "t017").await?;
    s.executor.turn_started(&thread, "turn-1").await?;
    let mut ticket = s
        .store
        .ticket("t017")
        .await?
        .ok_or_else(|| anyhow::anyhow!("t017"))?;
    ticket.inputs.push(TicketInput::Trigger {
        id: "i2".into(),
        trigger_id: "tr1".into(),
        at: "4".into(),
        note: None,
        human: Some(true),
        context: None,
        read_at: None,
        delivered_to: None,
        delivered_at: None,
    });
    ticket.undelivered.push("i2".into());
    let assignment = latest_assignment(&ticket)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no assignment"))?;
    s.store
        .atomic(async |tx| {
            tx.save_ticket(&ticket).await?;
            s.executor.deliver(tx, &ticket, &assignment).await
        })
        .await?;
    s.effects.idle(None).await;
    assert!(
        s.host
            .calls()
            .last()
            .is_some_and(|c| c.contains(":Steer:[输入 i2]")),
        "{:?}",
        s.host.calls()
    );
    Ok(())
}

#[tokio::test]
async fn a_ticket_opened_from_a_handback_starts_with_the_origin_tickets_context() -> anyhow::Result<()> {
    let s = setup().await?;
    // t010 did the original work and handed the follow-up (trigger tr1) back.
    let mut origin = ticket("t010");
    origin.title = "修正样例下界".into();
    origin.mandate.scope = "修正 tolerance.py 的上下界计算".into();
    origin.origin.trigger_id = "tr0".into();
    s.store
        .atomic(async |tx| {
            tx.save_trigger(&trigger(
                "tr0",
                "codext",
                &[("codext", "pm/TR-0")],
                "0",
                "请做：修 ~/237-codext-org/samples/tolerance/tolerance.py 的下界计算",
            ))
            .await?;
            tx.save_ticket(&origin).await?;
            tx.save_disposition(
                "tr1",
                &TriggerDisposition {
                    fanout: vec![origin.id.clone()],
                    handbacks: Some(vec![Handback {
                        ticket_id: origin.id.clone(),
                        reason: "要改测试，超出授权范围".into(),
                        at: "2026-11-03T08:00:00Z".into(),
                    }]),
                    ..TriggerDisposition::default()
                },
            )
            .await
        })
        .await?;
    launch(&s, &s.executor, &s.effects, &ticket("t017")).await?;
    let first = s
        .host
        .texts
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no delivery"))?;
    assert!(first.contains("退回自工单 t010（理由：要改测试，超出授权范围）"), "{first}");
    assert!(first.contains("原工单授权范围：修正 tolerance.py 的上下界计算"), "{first}");
    assert!(first.contains("samples/tolerance/tolerance.py"), "{first}");
    Ok(())
}

#[tokio::test]
async fn a_pms_project_in_the_thread_list_decides_where_its_tickets_worktrees_live()
-> anyhow::Result<()> {
    // One org, two projects: the FEM PM's tickets come from the FEM repository.
    let (dir, store) = open_store().await?;
    let clock = FakeClock::at(T0);
    let host = Arc::new(FakeHost::default());
    let engine = Arc::new(FakeEngine::default());
    let effects = Effects::new(store.clone(), HealthBoard::new(), clock.clock()).await?;
    effects.start();
    let pm_file = dir.path().join("superagent/pm-threads");
    std::fs::create_dir_all(pm_file.parent().expect("parent"))?;
    std::fs::write(
        &pm_file,
        format!(
            "# PMs\npm-codext\npm-fem project=fem repo={} base=main worktrees={}\n",
            dir.path().join("fem-repo").display(),
            dir.path().join("wt-fem").display()
        ),
    )?;
    let mut cfg = config(&dir, 4);
    cfg.pm_threads_file = Some(pm_file);
    let executor = install_executor(
        store.clone(),
        effects.clone(),
        engine.clone() as Arc<dyn EngineForExecutor>,
        host.clone() as Arc<dyn SuperagentHost>,
        cfg.clone(),
        clock.clock(),
    )
    .await?;
    let s = Setup {
        _dir: dir,
        store,
        clock,
        host,
        engine,
        effects,
        executor,
        config: cfg,
    };
    let channel = |pm: &str| ChannelRef {
        source: "codext".into(),
        thread: pm.into(),
        address: Default::default(),
    };
    let mut fem = ticket("t017");
    fem.channels = vec![channel("pm-fem")];
    launch(&s, &s.executor, &s.effects, &fem).await?;
    let mut other = ticket("t021");
    other.channels = vec![channel("pm-codext")];
    launch(&s, &s.executor, &s.effects, &other).await?;
    let calls = s.host.calls();
    let fem_wt = s._dir.path().join("wt-fem").join("t017");
    let other_wt = s.config.worktree_root.join("t021");
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("start_thread:") && c.ends_with(&fem_wt.display().to_string())),
        "{calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("start_thread:") && c.ends_with(&other_wt.display().to_string())),
        "{calls:?}"
    );
    Ok(())
}
