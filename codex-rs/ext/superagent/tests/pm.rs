//! The PM's tools and the delivery of candidate cards to the PM thread, against a fake engine and
//! a fake host (ported from the PM parts of the prototype's `tests/tools.spec.ts`).

mod common;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use codex_protocol::ThreadId;
use codex_superagent_extension::core::ChannelRef;
use codex_superagent_extension::BoxFut;
use codex_superagent_extension::Effects;
use codex_superagent_extension::HealthBoard;
use codex_superagent_extension::core::AcceptMode;
use codex_superagent_extension::core::AssignmentState;
use codex_superagent_extension::core::Decision;
use codex_superagent_extension::core::DecisionKind;
use codex_superagent_extension::core::LastActivity;
use codex_superagent_extension::core::LastActivityKind;
use codex_superagent_extension::core::Reason;
use codex_superagent_extension::core::Strength;
use codex_superagent_extension::core::Thread;
use codex_superagent_extension::core::Ticket;
use codex_superagent_extension::core::UserError;
use codex_superagent_extension::host::DeliveryMode;
use codex_superagent_extension::host::GoalStatus;
use codex_superagent_extension::host::SessionBrief;
use codex_superagent_extension::host::SessionPage;
use codex_superagent_extension::host::SuperagentHost;
use codex_superagent_extension::host::ThreadSpec;
use codex_superagent_extension::host::TurnBrief;
use codex_superagent_extension::pm::Candidate;
use codex_superagent_extension::pm::EngineError;
use codex_superagent_extension::pm::EngineForPm;
use codex_superagent_extension::pm::PmRuntime;
use codex_superagent_extension::pm::PmThreads;
use codex_superagent_extension::pm::RouteOutcome;
use codex_superagent_extension::pm::install_pm_handlers;
use codex_superagent_extension::pm::pm_tools;
use codex_superagent_extension::resources::ReadScope;
use codex_superagent_extension::tools::NOTE_WORDS;
use codex_superagent_extension::tools::pm::PmTool;
use codex_superagent_extension::tools::pm::PmToolKind;
use codex_superagent_extension::tools::read::ReadTool;
use common::FakeClock;
use common::T0;
use common::open_store;
use common::thread;
use common::ticket;
use common::trigger;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

/// Fake engine: accepts create tickets in the store, named `t017_<worktree>`; attaches and stops are
/// scripted; every call is recorded.
#[derive(Default)]
struct FakeEngine {
    store: Option<codex_superagent_extension::Store>,
    calls: Mutex<Vec<String>>,
    candidate: Mutex<Option<Candidate>>,
    /// Trigger ids already decided, with their decision ids, for the "already accepted" path.
    decided: Mutex<Vec<(String, Decision)>>,
    fail_attach_with: Mutex<Option<String>>,
}

impl FakeEngine {
    fn record(&self, call: String) {
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
}

fn decision(
    id: &str,
    kind: DecisionKind,
    trigger_id: &str,
    ticket_id: Option<&str>,
    reason: Reason,
) -> Decision {
    Decision {
        id: id.to_owned(),
        eval_id: "eval-1".into(),
        thread_key: "codext:pm/TR-1".into(),
        trigger_ids: vec![trigger_id.to_owned()],
        policy_version: String::new(),
        at: "1".into(),
        kind,
        reason: vec![reason],
        strength: Strength::Explicit,
        ticket_id: ticket_id.map(str::to_owned),
        mandate: None,
        audit: None,
        dismissed: None,
    }
}

impl EngineForPm for FakeEngine {
    fn candidate(&self, trigger_id: String) -> BoxFut<'_, Result<Option<Candidate>, EngineError>> {
        self.record(format!("candidate {trigger_id}"));
        let candidate = self
            .candidate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        Box::pin(async move { Ok(candidate) })
    }

    fn candidate_delivered(&self, eval_id: String) -> BoxFut<'_, Result<(), EngineError>> {
        self.record(format!("candidate_delivered {eval_id}"));
        Box::pin(async { Ok(()) })
    }

    fn decide(
        &self,
        trigger_id: String,
        outcome: RouteOutcome,
    ) -> BoxFut<'_, Result<Decision, EngineError>> {
        self.record(format!("decide {trigger_id} {outcome:?}"));
        Box::pin(async move {
            if let Some((_, earlier)) = self
                .decided
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .find(|(id, _)| *id == trigger_id)
            {
                return Ok(earlier.clone());
            }
            let decision = match outcome {
                RouteOutcome::Accept { title, name, .. } => {
                    let ticket_id = format!("t017_{}", name.unwrap_or_default().replace('-', "_"));
                    let mut created = ticket(&ticket_id, false, &[AssignmentState::Pending], &[]);
                    created.title = title;
                    if let Some(store) = &self.store {
                        store
                            .atomic(async |tx| tx.save_ticket(&created).await)
                            .await?;
                    }
                    let decision = decision(
                        "d-accept",
                        DecisionKind::Accept,
                        &trigger_id,
                        Some(&ticket_id),
                        Reason::code("explicitRouted"),
                    );
                    self.decided
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push((trigger_id.clone(), decision.clone()));
                    decision
                }
                RouteOutcome::Stop { ticket_id, reason } => decision(
                    "d-stop",
                    DecisionKind::Stop,
                    &trigger_id,
                    Some(&ticket_id),
                    reason,
                ),
                RouteOutcome::Ignore { reason } => {
                    decision("d-ignore", DecisionKind::Ignore, &trigger_id, None, reason)
                }
            };
            Ok(decision)
        })
    }

    fn attach(
        &self,
        ticket_id: String,
        trigger_id: String,
        note: Option<String>,
    ) -> BoxFut<'_, Result<Ticket, EngineError>> {
        self.record(format!("attach {ticket_id} {trigger_id} {note:?}"));
        Box::pin(async move {
            if let Some(err) = self
                .fail_attach_with
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
            {
                return Err(EngineError::Other(err));
            }
            let Some(store) = &self.store else {
                return Err(EngineError::Other("no store".into()));
            };
            match store.ticket(&ticket_id).await? {
                Some(ticket) => Ok(ticket),
                None => Err(UserError::new("unknownTicket", &[]).into()),
            }
        })
    }

    fn request_status(&self, ticket_id: String) -> BoxFut<'_, Result<bool, EngineError>> {
        self.record(format!("request_status {ticket_id}"));
        Box::pin(async { Ok(false) })
    }

    fn note(&self, ticket_id: String, text: String) -> BoxFut<'_, Result<(), EngineError>> {
        self.record(format!("note {ticket_id} {text}"));
        Box::pin(async { Ok(()) })
    }

    fn retry_ticket(&self, ticket_id: String) -> BoxFut<'_, Result<Ticket, EngineError>> {
        self.record(format!("retry {ticket_id}"));
        Box::pin(async move {
            let Some(store) = &self.store else {
                return Err(EngineError::Other("no store".into()));
            };
            match store.ticket(&ticket_id).await? {
                Some(ticket) => Ok(ticket),
                None => Err(UserError::new("unknownTicket", &[]).into()),
            }
        })
    }

    fn reevaluate(&self, trigger_id: String) -> BoxFut<'_, Result<bool, EngineError>> {
        self.record(format!("reevaluate {trigger_id}"));
        Box::pin(async { Ok(true) })
    }
}

/// Fake host: records what reaches which thread.
#[derive(Default)]
struct FakeHost {
    delivered: Mutex<Vec<(String, String, DeliveryMode)>>,
}

impl SuperagentHost for FakeHost {
    fn start_thread(&self, _spec: ThreadSpec) -> BoxFut<'_, anyhow::Result<ThreadId>> {
        Box::pin(async { anyhow::bail!("not in this test") })
    }
    fn bind_profile(&self, _thread: ThreadId, _profile: String) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn resume_thread(
        &self,
        _thread: ThreadId,
        _role: Option<String>,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn deliver(
        &self,
        thread: ThreadId,
        text: String,
        mode: DeliveryMode,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        self.delivered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((thread.to_string(), text, mode));
        Box::pin(async { Ok(()) })
    }
    fn interrupt(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn clean_background(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn children(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<ThreadId>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn goal_set(
        &self,
        _thread: ThreadId,
        _objective: String,
        _token_budget: Option<i64>,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn goal_clear(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn background_commands(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<String>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn goal_status(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Option<GoalStatus>>> {
        Box::pin(async { Ok(None) })
    }
    fn list_sessions(
        &self,
        cursor: Option<String>,
        limit: u32,
    ) -> BoxFut<'_, anyhow::Result<SessionPage>> {
        // Two pages of one session each: the first names where the second starts.
        let page = match cursor.as_deref() {
            None => SessionPage {
                sessions: vec![SessionBrief {
                    id: SESSION.to_owned(),
                    name: Some("有限元重写".to_owned()),
                    preview: "询问项目\n当前进展".to_owned(),
                    cwd: "/Users/u/239-jtufem".to_owned(),
                    status: "notLoaded".to_owned(),
                    updated_at: Some(1_793_000_000),
                    model: Some("gpt-6-astra".to_owned()),
                    effort: Some("max".to_owned()),
                    account_profile: Some("Simmonsalex805".to_owned()),
                }],
                next_cursor: Some("c2".to_owned()),
            },
            Some(_) => SessionPage::default(),
        };
        let _ = limit;
        Box::pin(async move { Ok(page) })
    }
    fn session_turns(
        &self,
        _thread: ThreadId,
        limit: u32,
    ) -> BoxFut<'_, anyhow::Result<Vec<TurnBrief>>> {
        let turn = TurnBrief {
            id: "turn-9".to_owned(),
            status: "completed".to_owned(),
            user: vec!["现在进展到哪了？".to_owned()],
            assistant: vec!["长".repeat(700)],
            ..TurnBrief::default()
        };
        Box::pin(async move { Ok(std::iter::repeat_n(turn, limit as usize).collect()) })
    }
}

const SESSION: &str = "019a0000-0000-7000-8000-00000000c003";

#[tokio::test]
async fn the_router_reads_sessions_without_loading_them() -> anyhow::Result<()> {
    let (_dir, store) = open_store().await?;
    let host: Arc<dyn SuperagentHost> = Arc::new(FakeHost::default());
    let router = ReadTool::for_pm(store.clone(), host);
    let read = |args: Value| {
        let router = &router;
        async move {
            router
                .run(&args.to_string())
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))
        }
    };
    let page = read(json!({ "resource": "sessions" })).await?;
    assert_eq!(
        page,
        json!({
            "items": [{
                "id": SESSION,
                "name": "有限元重写",
                "preview": "询问项目 当前进展",
                "cwd": "/Users/u/239-jtufem",
                "status": "notLoaded",
                "updatedAt": 1_793_000_000,
                "model": "gpt-6-astra",
                "effort": "max",
                "account": "Simmonsalex805",
            }],
            "next_resource": "sessions/c2",
        })
    );
    let last = read(json!({ "resource": "sessions/c2" })).await?;
    assert_eq!(last, json!({ "items": [], "next_resource": null }));
    let turns = read(json!({ "resource": format!("session/{SESSION}"), "limit": 1 })).await?;
    assert_eq!(turns["turns"].as_array().map(Vec::len), Some(1));
    assert_eq!(turns["turns"][0]["user"], json!(["现在进展到哪了？"]));
    assert_eq!(
        turns["turns"][0]["assistant"][0]
            .as_str()
            .map(|text| text.chars().count()),
        Some(600),
        "long answers are shortened"
    );
    // A worker's read tool has no sessions to read.
    let worker = ReadTool::new(
        store,
        ReadScope::Worker {
            ticket_id: "t001".to_owned(),
        },
    );
    assert!(
        worker
            .run(&json!({ "resource": "sessions" }).to_string())
            .await
            .is_err()
    );
    Ok(())
}

struct Setup {
    _dir: TempDir,
    engine: Arc<FakeEngine>,
    host: Arc<FakeHost>,
    runtime: Arc<PmRuntime>,
    pm_thread: ThreadId,
}

async fn setup() -> anyhow::Result<Setup> {
    let (dir, store) = open_store().await?;
    let board = HealthBoard::new();
    let effects = Effects::new(store.clone(), board, FakeClock::at(T0).clock()).await?;
    effects.start();
    let engine = Arc::new(FakeEngine {
        store: Some(store.clone()),
        ..FakeEngine::default()
    });
    let host = Arc::new(FakeHost::default());
    let pm_thread = ThreadId::new();
    let pm_threads = PmThreads::under(dir.path());
    std::fs::create_dir_all(dir.path().join("superagent"))?;
    std::fs::write(pm_threads.path(), format!("# PM threads\n{pm_thread}\n"))?;
    let runtime = Arc::new(PmRuntime {
        engine: Arc::clone(&engine) as Arc<dyn EngineForPm>,
        host: Arc::clone(&host) as Arc<dyn SuperagentHost>,
        store,
        effects,
        pm_threads,
    });
    install_pm_handlers(Arc::clone(&runtime)).await?;
    Ok(Setup {
        _dir: dir,
        engine,
        host,
        runtime,
        pm_thread,
    })
}

async fn route(s: &Setup, decisions: Value) -> anyhow::Result<Vec<Value>> {
    let tool = PmTool::new(PmToolKind::Route, Arc::clone(&s.runtime));
    let result = tool
        .run(&json!({ "decisions": decisions }).to_string())
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    Ok(result["results"].as_array().cloned().unwrap_or_default())
}

async fn seed_trigger(s: &Setup, id: &str, text: &str) -> anyhow::Result<()> {
    let t = trigger(
        id,
        "codext",
        &[("codext", "pm/TR-1")],
        "2026-11-02T00:00:00.000Z",
        text,
    );
    s.runtime
        .store
        .atomic(async |tx| tx.save_trigger(&t).await)
        .await
}

#[tokio::test]
async fn one_route_call_accepts_a_candidate_as_a_ticket_and_records_an_ignore() -> anyhow::Result<()>
{
    let s = setup().await?;
    seed_trigger(&s, "TR-1", "修一下回归").await?;
    seed_trigger(&s, "TR-2", "谢谢").await?;
    let results = route(
        &s,
        json!([
            { "kind": "accept", "trigger_id": "TR-1", "title": "修回归", "worktree_name": "fix-regression", "acknowledgement": "修 thread/queue 的重复投递，加回归测试。" },
            { "kind": "ignore", "trigger_id": "TR-2", "reason": "只是道谢" },
        ]),
    )
    .await?;
    assert_eq!(results[0]["kind"], "accept");
    assert_eq!(results[0]["decision"], "accept");
    assert_eq!(results[0]["ticket"]["ticket_id"], "t017_fix_regression");
    assert_eq!(results[0]["ticket"]["title"], "修回归");
    assert_eq!(
        results[1],
        json!({ "trigger_id": "TR-2", "kind": "ignore", "decision": "ignore", "reason": "只是道谢" })
    );
    assert_eq!(s.runtime.store.tickets().await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn routes_several_items_in_order_and_answers_a_refused_or_failed_item_without_undoing_the_others()
-> anyhow::Result<()> {
    let s = setup().await?;
    for (id, text) in [
        ("TR-1", "修一下回归"),
        ("TR-2", "谢谢"),
        ("TR-3", "另一个问题"),
        ("TR-4", "顺便看看"),
        ("TR-6", "还有这个"),
    ] {
        seed_trigger(&s, id, text).await?;
    }
    let existing = ticket("t001_first", false, &[AssignmentState::Running], &[]);
    s.runtime
        .store
        .atomic(async |tx| tx.save_ticket(&existing).await)
        .await?;
    let results = route(
        &s,
        json!([
            { "kind": "ignore", "trigger_id": "TR-2", "reason": "Thanks only." },
            { "kind": "stop", "trigger_id": "TR-3" },
            { "kind": "accept", "trigger_id": "TR-3", "title": "Other", "worktree_name": "fix-other", "acknowledgement": "收到。" },
            { "kind": "attach", "trigger_id": "TR-4", "ticket_id": "no-such-ticket" },
            { "kind": "attach", "trigger_id": "TR-6", "ticket_id": "t001_first" },
        ]),
    )
    .await?;
    assert_eq!(
        results[0],
        json!({ "trigger_id": "TR-2", "kind": "ignore", "decision": "ignore", "reason": "Thanks only." })
    );
    assert_eq!(
        results[1],
        json!({ "trigger_id": "TR-3", "kind": "stop", "refused": "stop needs ticket_id" })
    );
    assert_eq!(results[2]["decision"], "accept");
    assert_eq!(results[2]["ticket"]["title"], "Other");
    assert_eq!(
        results[3],
        json!({ "trigger_id": "TR-4", "kind": "attach", "refused": "Unknown ticket" })
    );
    assert_eq!(
        results[4]["reason"],
        "Attached: the ticket follows the trigger's thread and receives the trigger"
    );
    assert_eq!(results[4]["ticket"]["ticket_id"], "t001_first");
    // An unexpected failure recorded nothing, which the item may be sent again for.
    *s.engine
        .fail_attach_with
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some("database is locked".into());
    let results = route(
        &s,
        json!([{ "kind": "attach", "trigger_id": "TR-6", "ticket_id": "t001_first" }]),
    )
    .await?;
    assert_eq!(
        results[0]["refused"],
        "Not recorded: database is locked; retry this item"
    );
    Ok(())
}

#[tokio::test]
async fn refuses_alone_an_item_that_lacks_a_field_or_breaks_a_limit_and_ignores_unused_fields()
-> anyhow::Result<()> {
    let s = setup().await?;
    let words = (0..NOTE_WORDS + 1)
        .map(|_| "word")
        .collect::<Vec<_>>()
        .join(" ");
    let results = route(
        &s,
        json!([
            { "kind": "accept", "trigger_id": "a", "title": "Fix", "acknowledgement": "收到。" },
            { "kind": "accept", "trigger_id": "b", "title": "Fix", "acknowledgement": "收到。", "worktree_name": "one-two-three-four-five" },
            { "kind": "accept", "trigger_id": "c", "title": "Fix", "acknowledgement": "收到。", "worktree_name": "fix-c", "note": words },
            { "kind": "accept", "trigger_id": "d", "title": "", "acknowledgement": "收到。", "worktree_name": "fix-d" },
            { "kind": "ignore", "trigger_id": "e", "reason": "chat", "ticket_id": null, "title": null, "note": null },
        ]),
    )
    .await?;
    assert_eq!(results[0]["refused"], "accept needs worktree_name");
    assert!(
        results[1]["refused"]
            .as_str()
            .unwrap_or_default()
            .starts_with("worktree_name: ")
    );
    assert!(
        results[2]["refused"]
            .as_str()
            .unwrap_or_default()
            .starts_with("note: ")
    );
    assert!(
        results[2]["refused"]
            .as_str()
            .unwrap_or_default()
            .contains(&format!("{NOTE_WORDS} words"))
    );
    assert_eq!(results[3]["refused"], "accept needs title");
    assert_eq!(
        results[4],
        json!({ "trigger_id": "e", "kind": "ignore", "decision": "ignore", "reason": "chat" })
    );
    assert!(s.runtime.store.tickets().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_second_accept_keeps_the_first_and_an_ignore_of_a_decided_trigger_is_refused()
-> anyhow::Result<()> {
    let s = setup().await?;
    seed_trigger(&s, "TR-1", "首页的标题").await?;
    let accept = json!({ "kind": "accept", "trigger_id": "TR-1", "title": "Retitle", "worktree_name": "retitle", "acknowledgement": "收到。" });
    let first = route(&s, json!([accept])).await?;
    let ticket_id = first[0]["ticket"]["ticket_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    // The disposition now names the decision, as the engine would have recorded it.
    let disposition = codex_superagent_extension::core::TriggerDisposition {
        decision_id: Some("d-accept".into()),
        at: String::new(),
        ..Default::default()
    };
    s.runtime
        .store
        .atomic(async |tx| tx.save_disposition("TR-1", &disposition).await)
        .await?;
    let again = route(
        &s,
        json!([accept, { "kind": "ignore", "trigger_id": "TR-1", "reason": "chat" }]),
    )
    .await?;
    assert_eq!(
        again[0]["reason"],
        format!("Already accepted as {ticket_id}; title and acknowledgement unchanged")
    );
    assert_eq!(
        again[1]["refused"],
        "Decided in this call with TR-1, whose Decision took its batch"
    );
    Ok(())
}

#[tokio::test]
async fn the_ticket_tools_answer_in_one_shape_with_the_card() -> anyhow::Result<()> {
    let s = setup().await?;
    let existing = ticket("t001_first", false, &[AssignmentState::Running], &[]);
    s.runtime
        .store
        .atomic(async |tx| tx.save_ticket(&existing).await)
        .await?;
    let run = |kind: PmToolKind, args: Value| {
        let runtime = Arc::clone(&s.runtime);
        async move {
            PmTool::new(kind, runtime)
                .run(&args.to_string())
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))
        }
    };
    let status = run(
        PmToolKind::RequestStatus,
        json!({ "ticket_id": "t001_first" }),
    )
    .await?;
    assert_eq!(
        status["reason"],
        "The worker is not waiting; nothing was sent"
    );
    assert_eq!(status["ticket"]["ticket_id"], "t001_first");
    let retried = run(
        PmToolKind::TicketRetry,
        json!({ "ticket_id": "t001_first" }),
    )
    .await?;
    assert_eq!(retried["reason"], "Started a new assignment");
    let noted = run(
        PmToolKind::TicketNote,
        json!({ "ticket_id": "t001_first", "text": "t002 found the cause in the queue" }),
    )
    .await?;
    assert_eq!(noted["reason"], "Noted for the worker");
    let too_long = run(
        PmToolKind::TicketNote,
        json!({ "ticket_id": "t001_first", "text": "word ".repeat(101) }),
    )
    .await;
    assert_eq!(
        too_long.map_err(|e| e.to_string()),
        Err("At most 100 words".to_owned())
    );
    seed_trigger(&s, "TR-9", "再看看").await?;
    let requeued = run(PmToolKind::TriggerRequeue, json!({ "trigger_id": "TR-9" })).await?;
    assert_eq!(requeued, json!({ "queued": true }));
    assert!(s.engine.calls().iter().any(|c| c == "reevaluate TR-9"));
    Ok(())
}

#[tokio::test]
async fn a_candidate_card_reaches_the_pm_thread_once_and_the_engine_hears_it_was_delivered()
-> anyhow::Result<()> {
    let s = setup().await?;
    let t = trigger(
        "TR-1",
        "codext",
        &[("codext", "pm/TR-1")],
        "2026-11-02T00:00:00.000Z",
        "修一下回归",
    );
    s.runtime
        .store
        .atomic(async |tx| tx.save_trigger(&t).await)
        .await?;
    let mut thread = thread("pm/TR-1", "codext");
    thread.subscriptions = Vec::new();
    *s.engine
        .candidate
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(Candidate {
        eval_id: "eval-1".into(),
        thread: Thread::new(thread.reference.clone(), ""),
        triggers: vec![t.clone()],
        strength: Strength::Explicit,
        policy_version: String::new(),
        tickets: Vec::new(),
        mode: AcceptMode::Observe,
    });
    s.runtime
        .effects
        .enqueue_now("assessment", "eval-1", &json!({ "triggerId": "TR-1" }))
        .await?;
    s.runtime.effects.idle(None).await;
    let delivered = s
        .host
        .delivered
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].0, s.pm_thread.to_string());
    assert_eq!(delivered[0].2, DeliveryMode::Steer);
    assert!(
        delivered[0]
            .1
            .starts_with("[候选卡 eval=eval-1] 触发 TR-1 · 强度 明确 · 模式 observe")
    );
    let calls = s.engine.calls();
    assert_eq!(
        calls
            .iter()
            .filter(|c| *c == "candidate_delivered eval-1")
            .count(),
        1
    );
    assert!(!s.runtime.effects.pending("assessment", None).await?);
    // A stale assessment, whose Eval ended, delivers nothing.
    s.runtime
        .effects
        .enqueue_now("assessment", "eval-0", &json!({ "triggerId": "TR-1" }))
        .await?;
    s.runtime.effects.idle(None).await;
    assert_eq!(
        s.host
            .delivered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
        1
    );
    // Relays and notices reach the PM in order.
    s.runtime
        .effects
        .enqueue_now(
            "relay",
            "report:1",
            &json!({ "text": "{\"kind\":\"worker_report\"}" }),
        )
        .await?;
    s.runtime
        .effects
        .enqueue_now(
            "notice",
            "ended:t1",
            &json!({ "value": { "ticket_ended": "t1" }, "summary": "Ticket t1 ended" }),
        )
        .await?;
    s.runtime.effects.idle(None).await;
    let delivered = s
        .host
        .delivered
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    // Relays and notices are two ordered kinds; between the kinds there is no order.
    assert_eq!(delivered.len(), 3);
    let later: Vec<Value> = delivered[1..]
        .iter()
        .map(|(_, text, _)| serde_json::from_str(text).unwrap_or_default())
        .collect();
    assert!(
        later.iter().any(|v| v["kind"] == "worker_report"),
        "{later:?}"
    );
    // A notice is a card the person can read in the PM's window too.
    assert!(
        delivered[1..]
            .iter()
            .any(|(_, text, _)| text == "[通知] Ticket t1 ended"),
        "{delivered:?}"
    );
    Ok(())
}

#[tokio::test]
async fn pm_threads_come_from_the_file_and_only_they_get_tools() -> anyhow::Result<()> {
    let s = setup().await?;
    assert!(s.runtime.pm_threads.is_pm(&s.pm_thread.to_string()));
    assert!(!s.runtime.pm_threads.is_pm("other"));
    assert_eq!(
        s.runtime.pm_threads.primary(),
        Some(s.pm_thread.to_string())
    );
    let names: Vec<String> = pm_tools(&s.runtime)
        .iter()
        .map(|t| t.tool_name().name)
        .collect();
    // The host's tool policy for PM threads allows ROUTER_TOOLS only: every tool a Router can be
    // given must be there, the drive tool included.
    let drive = codex_superagent_extension::tools::drive::DriveTool::new(Arc::new(|| None), /*caller*/ None);
    let mut offered = names.clone();
    offered.push(
        codex_extension_api::ToolExecutor::<codex_extension_api::ToolCall<'_>>::tool_name(&drive)
            .name,
    );
    let allowed: Vec<String> = codex_superagent_extension::pm::ROUTER_TOOLS
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    assert_eq!(offered, allowed);
    assert_eq!(
        names,
        vec![
            "superagent_read",
            "superagent_route",
            "superagent_ticket_note",
            "superagent_ticket_retry",
            "superagent_request_status",
            "superagent_trigger_requeue"
        ]
    );
    Ok(())
}

#[tokio::test]
async fn read_lists_pages_and_keeps_the_pm_resources_from_a_worker() -> anyhow::Result<()> {
    let s = setup().await?;
    let store = s.runtime.store.clone();
    let mut first = ticket("t001_first", false, &[AssignmentState::Running], &[]);
    first.last_activity = Some(LastActivity {
        at: "2026-01-01T00:00:00.000Z".into(),
        kind: LastActivityKind::TurnStarted,
        text: String::new(),
    });
    first.assignments[0].report = Some(codex_superagent_extension::core::Report {
        state: ReportState::Progress,
        summary: "字".repeat(5000),
        at: "1".into(),
    });
    let second = ticket("t002_second", false, &[AssignmentState::Pending], &[]);
    let t = trigger(
        "TR-1",
        "codext",
        &[("codext", "pm/TR-1")],
        "2026-11-02T00:00:00.000Z",
        "修一下回归",
    );
    store
        .atomic(async |tx| {
            tx.save_ticket(&first).await?;
            tx.save_ticket(&second).await?;
            tx.save_trigger(&t).await
        })
        .await?;
    let read = |scope: ReadScope, args: Value| {
        let store = store.clone();
        async move {
            ReadTool::new(store, scope)
                .run(&args.to_string())
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))
        }
    };
    let page = read(ReadScope::Pm, json!({ "resource": "tickets", "limit": 1 })).await?;
    assert_eq!(page["items"][0]["ticket_id"], "t002_second");
    assert_eq!(page["items"][0]["state"], "working/queued");
    assert_eq!(page["next_offset"], 1);
    let page = read(ReadScope::Pm, json!({ "resource": "tickets", "offset": 1 })).await?;
    assert_eq!(page["items"][0]["ticket_id"], "t001_first");
    assert_eq!(page["next_offset"], Value::Null);
    let details = read(ReadScope::Pm, json!({ "resource": "ticket/t001_first" })).await?;
    assert_eq!(details["ticket_id"], "t001_first");
    assert_eq!(details["assignments"]["items"][0]["state"], "running");
    assert_eq!(details["result"], "ticket/t001_first/result");
    assert_eq!(details["lastActivity"]["kind"], "turn_started");
    assert_eq!(details["lastActivity"]["text"], "");
    assert!(details["lastActivityMinutes"].is_number());
    let result = read(
        ReadScope::Pm,
        json!({ "resource": "ticket/t001_first/result" }),
    )
    .await?;
    assert_eq!(
        result["text"].as_str().map(|s| s.chars().count()),
        Some(4000)
    );
    assert_eq!(result["next_offset"], 4000);
    let rest = read(
        ReadScope::Pm,
        json!({ "resource": "ticket/t001_first/result", "offset": 4000, "limit": 8000 }),
    )
    .await?;
    assert_eq!(rest["text"].as_str().map(|s| s.chars().count()), Some(1000));
    assert_eq!(rest["next_offset"], Value::Null);
    let trigger_page = read(ReadScope::Pm, json!({ "resource": "trigger/TR-1" })).await?;
    assert_eq!(trigger_page["text"], "修一下回归");
    assert_eq!(trigger_page["trigger_id"], "TR-1");
    let worker = ReadScope::Worker {
        ticket_id: "t001_first".into(),
    };
    for resource in ["tickets", "triggers", "threads", "decisions"] {
        let refused = read(worker.clone(), json!({ "resource": resource })).await;
        assert_eq!(
            refused.map_err(|e| e.to_string()),
            Err("This resource belongs to the PM".to_owned())
        );
    }
    let own = read(worker.clone(), json!({ "resource": "ticket/t001_first" })).await?;
    assert_eq!(own["ticket_id"], "t001_first");
    assert!(own.get("decision_id").is_none());
    let other = read(worker.clone(), json!({ "resource": "ticket/t002_second" })).await;
    assert_eq!(
        other.map_err(|e| e.to_string()),
        Err("Ticket does not belong to this worker".to_owned())
    );
    let unknown = read(ReadScope::Pm, json!({ "resource": "ticket" })).await;
    assert_eq!(
        unknown.map_err(|e| e.to_string()),
        Err("Unknown context resource".to_owned())
    );
    Ok(())
}

use codex_superagent_extension::core::ReportState;

#[tokio::test]
async fn a_second_pms_message_gets_its_candidate_card_and_its_tickets_relays_in_its_own_thread()
-> anyhow::Result<()> {
    // 11/03: with two PMs, the first PM received and decided a card for the second PM's message.
    let s = setup().await?;
    let pm2 = ThreadId::new();
    std::fs::write(
        s.runtime.pm_threads.path(),
        format!("# PM threads\n{}\n{pm2} project=other\n", s.pm_thread),
    )?;
    let mut t = trigger(
        "TR-2",
        "codext",
        &[("codext", "pm/TR-2")],
        "2026-11-02T00:00:00.000Z",
        "另一个项目的请求",
    );
    t.channel = Some(ChannelRef {
        source: "codext".into(),
        thread: pm2.to_string(),
        address: Default::default(),
    });
    s.runtime
        .store
        .atomic(async |tx| tx.save_trigger(&t).await)
        .await?;
    let mut thread = thread("pm/TR-2", "codext");
    thread.subscriptions = Vec::new();
    *s.engine
        .candidate
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(Candidate {
        eval_id: "eval-2".into(),
        thread: Thread::new(thread.reference.clone(), ""),
        triggers: vec![t.clone()],
        strength: Strength::Explicit,
        policy_version: String::new(),
        tickets: Vec::new(),
        mode: AcceptMode::Observe,
    });
    s.runtime
        .effects
        .enqueue_now("assessment", "eval-2", &json!({ "triggerId": "TR-2" }))
        .await?;
    s.runtime
        .effects
        .enqueue_now("relay", "relay-2", &json!({ "text": "[结果] 给第二个 PM", "pm": pm2.to_string() }))
        .await?;
    s.runtime
        .effects
        .enqueue_now("relay", "relay-1", &json!({ "text": "[结果] 没写给谁" }))
        .await?;
    s.runtime.effects.idle(None).await;
    let delivered = s
        .host
        .delivered
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let to_pm2: Vec<&String> = delivered
        .iter()
        .filter(|(thread, _, _)| *thread == pm2.to_string())
        .map(|(_, text, _)| text)
        .collect();
    assert_eq!(to_pm2.len(), 2, "{delivered:?}");
    assert!(to_pm2.iter().any(|t| t.starts_with("[候选卡 eval=eval-2]")), "{delivered:?}");
    assert!(to_pm2.iter().any(|t| t.as_str() == "[结果] 给第二个 PM"), "{delivered:?}");
    assert!(
        delivered
            .iter()
            .any(|(thread, text, _)| *thread == s.pm_thread.to_string() && text == "[结果] 没写给谁"),
        "{delivered:?}"
    );
    Ok(())
}
