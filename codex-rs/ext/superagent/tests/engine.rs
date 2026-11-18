//! The engine core on a temporary SQLite store: intake, Evals, fan-out, resume and stop,
//! follow-ups, hand-backs, attach, direct decisions, automatic Input, pause and Policy changes
//! (ported from the prototype's `tests/core-engine.spec.ts`).

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

use codex_superagent_extension::Effects;
use codex_superagent_extension::HealthBoard;
use codex_superagent_extension::Store;
use codex_superagent_extension::core::AcceptOutcome;
use codex_superagent_extension::core::ActorRef;
use codex_superagent_extension::core::Assignment;
use codex_superagent_extension::core::AssignmentState;
use codex_superagent_extension::core::ChannelRef;
use codex_superagent_extension::core::Decision;
use codex_superagent_extension::core::DecisionKind;
use codex_superagent_extension::core::Engine;
use codex_superagent_extension::core::EngineHooks;
use codex_superagent_extension::core::EngineOptions;
use codex_superagent_extension::core::GateResult;
use codex_superagent_extension::core::LastActivityKind;
use codex_superagent_extension::core::Mandate;
use codex_superagent_extension::core::MandateBasis;
use codex_superagent_extension::core::Outcome;
use codex_superagent_extension::core::Policy;
use codex_superagent_extension::core::PolicyEffect;
use codex_superagent_extension::core::PrepareTicket;
use codex_superagent_extension::core::ReceiveResult;
use codex_superagent_extension::core::ReportState;
use codex_superagent_extension::core::RuleChange;
use codex_superagent_extension::core::RuleEffect;
use codex_superagent_extension::core::ScopeResult;
use codex_superagent_extension::core::SourceScope;
use codex_superagent_extension::core::Strength;
use codex_superagent_extension::core::Thread;
use codex_superagent_extension::core::ThreadRef;
use codex_superagent_extension::core::Ticket;
use codex_superagent_extension::core::TicketEndBy;
use codex_superagent_extension::core::TicketInput;
use codex_superagent_extension::core::Trigger;
use codex_superagent_extension::core::WorkerReport;
use codex_superagent_extension::core::refusal;
use codex_superagent_extension::core::running_assignment;
use codex_superagent_extension::core::thread_rules;
use codex_superagent_extension::core::waits_for_person;
use codex_superagent_extension::ids::key;
use codex_superagent_extension::store::MessageKind;
use common::open_store;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

fn member() -> ActorRef {
    ActorRef {
        source: "chat".into(),
        id: "alice".into(),
    }
}

fn guest() -> ActorRef {
    ActorRef {
        source: "chat".into(),
        id: "guest".into(),
    }
}

fn chat(id: &str) -> ThreadRef {
    ThreadRef {
        source: "chat".into(),
        id: id.into(),
    }
}

fn trigger(id: &str) -> Trigger {
    Trigger {
        id: id.to_owned(),
        source: "chat".into(),
        threads: vec![chat("t1")],
        actor: member(),
        human: true,
        addressed: None,
        at: format!("2026-09-23T00:00:{id:0>2}Z"),
        text: format!("message {id}"),
        channel: Some(ChannelRef {
            source: "chat".into(),
            thread: "t1".into(),
            address: [("message".to_owned(), id.to_owned())]
                .into_iter()
                .collect(),
        }),
        url: None,
        meta: serde_json::Map::new(),
    }
}

fn on(mut t: Trigger, threads: &[ThreadRef]) -> Trigger {
    t.threads = threads.to_vec();
    t
}

fn by(mut t: Trigger, actor: ActorRef) -> Trigger {
    t.actor = actor;
    t
}

fn bot(mut t: Trigger) -> Trigger {
    t.human = false;
    t
}

/// The gate of the prototype's test harness: Alice is listed and explicit, a guest is a follow-up
/// where a thread has tickets, a bot is never admitted.
struct TestPolicy {
    version: Mutex<String>,
    paused: AtomicBool,
    /// Which rules admit an inferred accept; `None` admits any non-empty list.
    admits: Mutex<Option<Vec<String>>>,
}

impl TestPolicy {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            version: Mutex::new("v1".into()),
            paused: AtomicBool::new(false),
            admits: Mutex::new(None),
        })
    }

    fn set_version(&self, version: &str) {
        *self.version.lock().unwrap_or_else(PoisonError::into_inner) = version.to_owned();
    }

    fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::SeqCst);
    }
}

impl Policy for TestPolicy {
    fn version(&self) -> String {
        self.version
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    fn eval_timeout_ms(&self) -> i64 {
        60_000
    }

    fn gate(&self, trigger: &Trigger, thread: &Thread, _tickets: &[Ticket]) -> GateResult {
        if !trigger.human {
            GateResult::Inadmissible {
                reason: "not human".into(),
            }
        } else if trigger.actor.id == "alice" {
            GateResult::Admissible {
                strength: Strength::Explicit,
                basis: MandateBasis::via("listed"),
            }
        } else if !thread.subscriptions.is_empty() {
            GateResult::Admissible {
                strength: Strength::Inferred,
                basis: MandateBasis::via("followup"),
            }
        } else {
            GateResult::Inadmissible {
                reason: "not a member".into(),
            }
        }
    }

    fn revalidate(&self, _mandate: &Mandate) -> bool {
        self.version() != "revoked"
    }

    fn admits_inferred(&self, rules: &[String]) -> bool {
        match &*self.admits.lock().unwrap_or_else(PoisonError::into_inner) {
            None => !rules.is_empty(),
            Some(admitted) => rules.iter().any(|r| admitted.contains(r)),
        }
    }
}

struct TestScope;

impl SourceScope for TestScope {
    fn scope(&self, trigger: &Trigger) -> ScopeResult {
        if trigger.meta.get("blocked") == Some(&Value::Bool(true)) {
            ScopeResult::Filtered {
                reason: "chat not allowed".into(),
            }
        } else {
            ScopeResult::Listened { observed: false }
        }
    }
}

#[derive(Default)]
struct Hooks {
    stopped: Mutex<Vec<(String, String, String)>>,
    released: Mutex<Vec<String>>,
}

impl EngineHooks for Hooks {
    fn assignment_stopped(&self, ticket: &Ticket, assignment: &Assignment, reason: &str) {
        self.stopped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((ticket.id.clone(), assignment.id.clone(), reason.to_owned()));
    }

    fn eval_released(&self, eval_id: &str) {
        self.released
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(eval_id.to_owned());
    }
}

/// A candidate the engine asked the PM for, from its `assessment` effect.
#[derive(Clone, Debug)]
struct Req {
    eval_id: String,
    trigger_id: String,
    trigger_ids: Vec<String>,
    strength: String,
    thread_key: String,
}

struct H {
    _dir: TempDir,
    store: Store,
    effects: Effects,
    engine: Engine,
    policy: Arc<TestPolicy>,
    hooks: Arc<Hooks>,
    clock: Arc<AtomicI64>,
    /// Inputs the executor handed to a running assignment, as (ticket id, input id).
    delivered: Vec<(String, String)>,
}

async fn harness(prepare: Option<PrepareTicket>) -> anyhow::Result<H> {
    let (dir, store) = open_store().await?;
    let effects = Effects::new(store.clone(), HealthBoard::new(), Arc::new(|| 0i64)).await?;
    let policy = TestPolicy::new();
    let hooks = Arc::new(Hooks::default());
    let clock = Arc::new(AtomicI64::new(0));
    let now_clock = Arc::clone(&clock);
    let engine = Engine::new(EngineOptions {
        store: store.clone(),
        effects: effects.clone(),
        policy: policy.clone(),
        scope: Arc::new(TestScope),
        hooks: hooks.clone(),
        now: Some(Arc::new(move || {
            let seconds = now_clock.load(Ordering::SeqCst);
            format!("2026-09-23T00:{:02}:{:02}.000Z", seconds / 60, seconds % 60)
        })),
        prepare_ticket: prepare,
        pm: "pm-test".into(),
    });
    Ok(H {
        _dir: dir,
        store,
        effects,
        engine,
        policy,
        hooks,
        clock,
        delivered: Vec::new(),
    })
}

impl H {
    fn tick(&self, seconds: i64) {
        self.clock.fetch_add(seconds, Ordering::SeqCst);
    }

    async fn cancel(&self, kind: &str, key: &str) -> anyhow::Result<()> {
        let effects = self.effects.clone();
        self.store
            .atomic(async |tx| effects.cancel(tx, kind, key).await)
            .await
    }

    /// Simulates the executor: starts queued assignments (unless `launch` is false) and delivers waiting Inputs.
    async fn settle(&mut self, launch: bool) -> anyhow::Result<()> {
        loop {
            if launch
                && let Some((key, payload)) =
                    self.effects.queued("launch").await?.into_iter().next()
            {
                self.cancel("launch", &key).await?;
                let ticket = payload["ticketId"].as_str().unwrap_or_default().to_owned();
                let assignment = payload["assignmentId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                self.engine.assignment_started(&ticket, &assignment).await?;
                continue;
            }
            let Some((ticket_id, _)) = self.effects.queued("delivery").await?.into_iter().next()
            else {
                return Ok(());
            };
            self.cancel("delivery", &ticket_id).await?;
            let Some(ticket) = self.store.ticket(&ticket_id).await? else {
                continue;
            };
            let Some(assignment) = running_assignment(&ticket).cloned() else {
                continue;
            };
            if ticket.ended.is_some() {
                continue;
            }
            for input in ticket.undelivered.clone() {
                self.delivered.push((ticket_id.clone(), input.clone()));
                self.engine
                    .inputs_delivered(&ticket_id, &assignment.id, &[input])
                    .await?;
            }
        }
    }

    async fn pump(&mut self) -> anyhow::Result<()> {
        self.engine.evaluate_due().await?;
        self.settle(true).await
    }

    async fn requests(&self) -> anyhow::Result<Vec<Req>> {
        Ok(self
            .effects
            .queued("assessment")
            .await?
            .into_iter()
            .map(|(eval_id, payload)| Req {
                eval_id,
                trigger_id: payload["triggerId"].as_str().unwrap_or_default().to_owned(),
                trigger_ids: payload["triggerIds"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
                strength: payload["strength"].as_str().unwrap_or_default().to_owned(),
                thread_key: payload["threadKey"].as_str().unwrap_or_default().to_owned(),
            })
            .collect())
    }

    async fn last_request(&self) -> anyhow::Result<Req> {
        self.requests()
            .await?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("no candidate was requested"))
    }

    async fn decide(&self, req: &Req, outcome: Outcome) -> anyhow::Result<Decision> {
        self.engine
            .decide(&req.trigger_id, outcome, Some(&req.eval_id))
            .await
    }

    async fn accepted_on(&mut self, id: &str, thread: &str) -> anyhow::Result<Decision> {
        self.engine
            .receive(on(trigger(id), &[chat(thread)]))
            .await?;
        self.pump().await?;
        let req = self.last_request().await?;
        let decision = self
            .decide(
                &req,
                Outcome::Accept(AcceptOutcome::new("Fix", "small", "requested").acknowledge("ok")),
            )
            .await?;
        self.settle(true).await?;
        Ok(decision)
    }

    async fn accepted(&mut self) -> anyhow::Result<Decision> {
        self.accepted_on("1", "t1").await
    }

    async fn ticket(&self, id: &str) -> anyhow::Result<Ticket> {
        self.store
            .ticket(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no ticket {id}"))
    }

    async fn thread(&self, key: &str) -> anyhow::Result<Thread> {
        self.store
            .thread(key)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no thread {key}"))
    }

    async fn decisions(&self, thread_key: &str) -> anyhow::Result<Vec<Decision>> {
        self.store.decisions_by_thread(thread_key).await
    }

    fn stops(&self) -> usize {
        self.hooks
            .stopped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

fn accept(title: &str) -> Outcome {
    Outcome::Accept(AcceptOutcome::new(title, "small", "requested"))
}

fn code(result: &anyhow::Result<impl std::fmt::Debug>) -> String {
    match result {
        Ok(value) => format!("ok: {value:?}"),
        Err(err) => refusal(err)
            .map(|e| e.code.clone())
            .unwrap_or_else(|| format!("other: {err:#}")),
    }
}

fn ticket_id(decision: &Decision) -> String {
    decision.ticket_id.clone().unwrap_or_default()
}

fn input_ids(ticket: &Ticket) -> Vec<String> {
    ticket.inputs.iter().map(|i| i.id().to_owned()).collect()
}

fn trigger_inputs(ticket: &Ticket) -> Vec<String> {
    ticket
        .inputs
        .iter()
        .filter_map(|i| i.trigger_id().map(str::to_owned))
        .collect()
}

// ---- intake ----

#[tokio::test]
async fn stores_a_trigger_once_and_records_source_filtering() -> anyhow::Result<()> {
    let h = harness(None).await?;
    assert_eq!(h.engine.receive(trigger("1")).await?, ReceiveResult::Stored);
    assert_eq!(
        h.engine.receive(trigger("1")).await?,
        ReceiveResult::Duplicate
    );
    let mut blocked = trigger("2");
    blocked.meta.insert("blocked".into(), Value::Bool(true));
    assert_eq!(h.engine.receive(blocked).await?, ReceiveResult::Filtered);
    let filtered = h
        .store
        .disposition("2")
        .await?
        .and_then(|d| d.filtered)
        .map(|r| r.code);
    assert_eq!(filtered, Some("chat not allowed".to_owned()));
    assert_eq!(h.thread("chat:t1").await?.pending, vec!["1"]);
    Ok(())
}

#[tokio::test]
async fn records_correlation_across_threads_and_evaluates_only_on_the_primary_one()
-> anyhow::Result<()> {
    let h = harness(None).await?;
    let gh = |id: &str| ThreadRef {
        source: "gh".into(),
        id: id.into(),
    };
    h.engine
        .receive(on(trigger("1"), &[gh("pr-7"), gh("run-9")]))
        .await?;
    let pr = h.thread("gh:pr-7").await?;
    assert_eq!(
        (pr.pending, pr.correlated),
        (vec!["1".to_owned()], vec!["gh:run-9".to_owned()])
    );
    let run = h.thread("gh:run-9").await?;
    assert_eq!(
        (run.pending, run.correlated),
        (vec![], vec!["gh:pr-7".to_owned()])
    );
    Ok(())
}

// ---- eval ----

#[tokio::test]
async fn ignores_at_the_gate_without_an_assessor_when_nothing_is_admissible() -> anyhow::Result<()>
{
    let mut h = harness(None).await?;
    h.engine.receive(bot(trigger("1"))).await?;
    h.pump().await?;
    assert!(h.requests().await?.is_empty());
    let decisions = h.decisions("chat:t1").await?;
    assert_eq!(decisions.len(), 1);
    assert_eq!(
        (decisions[0].kind, decisions[0].trigger_ids.clone()),
        (DecisionKind::Ignore, vec!["1".to_owned()])
    );
    assert!(
        h.store
            .disposition("1")
            .await?
            .and_then(|d| d.decision_id)
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn releases_the_assessors_state_for_every_eval_that_ends() -> anyhow::Result<()> {
    let mut h = harness(None).await?;
    h.engine.receive(trigger("1")).await?;
    h.pump().await?;
    let req = h.last_request().await?;
    h.decide(&req, Outcome::ignore("chatter")).await?;
    assert_eq!(
        *h.hooks
            .released
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
        vec![req.eval_id]
    );
    Ok(())
}

#[tokio::test]
async fn serializes_evals_per_thread_and_debounces_triggers_that_arrive_meanwhile()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    h.engine.receive(trigger("1")).await?;
    h.pump().await?;
    h.engine.receive(trigger("2")).await?;
    h.engine.receive(trigger("3")).await?;
    h.pump().await?;
    assert_eq!(h.requests().await?.len(), 1);
    assert_eq!(h.thread("chat:t1").await?.pending, vec!["2", "3"]);
    let first = h.last_request().await?;
    h.decide(&first, Outcome::ignore("chatter")).await?;
    h.pump().await?;
    let second = h.last_request().await?;
    assert_eq!(
        (second.trigger_ids, second.strength.as_str()),
        (vec!["2".to_owned(), "3".to_owned()], "explicit")
    );
    Ok(())
}

#[tokio::test]
async fn accept_creates_a_ticket_with_a_mandate_subscribes_the_thread_queues_the_acknowledgement_and_starts_an_assignment()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let decision = h.accepted().await?;
    let ticket = h.ticket(&ticket_id(&decision)).await?;
    assert_eq!(ticket.title, "Fix");
    assert_eq!(
        (
            ticket.mandate.strength,
            ticket.mandate.policy_version.as_str(),
            ticket.mandate.basis.via.as_str(),
            ticket.mandate.scope.as_str()
        ),
        (Strength::Explicit, "v1", "listed", "small")
    );
    assert_eq!(ticket.mandate.actor, member());
    assert_eq!(
        ticket.channels,
        vec![trigger("1").channel.unwrap_or_default()]
    );
    assert_eq!(
        (
            ticket.origin.trigger_id.as_str(),
            ticket.origin.trigger_ids.clone()
        ),
        ("1", vec!["1".to_owned()])
    );
    assert!(ticket.ended.is_none());
    assert_eq!(ticket.assignments.len(), 1);
    assert_eq!(ticket.assignments[0].state, AssignmentState::Running);
    assert!(ticket.assignments[0].started_at.is_some());
    assert_eq!(
        h.thread("chat:t1").await?.subscriptions,
        vec![ticket.id.clone()]
    );
    // Its own Thread, for messages that name it or are said to its worker.
    let own = h.thread(&format!("codext:ticket/{}", ticket.id)).await?;
    assert_eq!(own.subscriptions, vec![ticket.id.clone()]);
    let ack = h
        .store
        .receipt(&key(["ack", "1"]))
        .await?
        .ok_or_else(|| anyhow::anyhow!("no acknowledgement"))?;
    assert_eq!(
        (
            ack.message.text.as_str(),
            ack.message.kind,
            ack.message.ticket_id.clone()
        ),
        ("ok", MessageKind::Acknowledgement, Some(ticket.id.clone()))
    );
    assert!(h.effects.pending("reply", Some(&ack.message.id)).await?);
    // Numbered for the PM, and findable by that name.
    assert_eq!(
        ticket.execution.as_ref().map(|e| e.name.as_str()),
        Some("t001_fix")
    );
    assert_eq!(
        h.engine.ticket_of_name("t001_fix").await?,
        Some(ticket.id.clone())
    );
    assert!(h.effects.queued("launch").await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn refuses_to_decide_an_eval_that_a_later_eval_replaced_and_decides_the_later_one()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    h.engine.receive(trigger("1")).await?;
    h.pump().await?;
    let first = h.last_request().await?;
    h.policy.set_version("v2");
    h.engine.policy_changed("v1", "v2", HashMap::new()).await?;
    h.pump().await?;
    let second = h.last_request().await?;
    assert_ne!(first.eval_id, second.eval_id);
    assert_eq!(
        code(&h.decide(&first, Outcome::ignore("chatter")).await),
        "evalFinished"
    );
    let decision = h.decide(&second, Outcome::ignore("chatter")).await?;
    assert_eq!(
        (decision.kind, decision.eval_id),
        (DecisionKind::Ignore, second.eval_id)
    );
    Ok(())
}

#[tokio::test]
async fn records_an_accept_with_its_ticket_subscriptions_and_acknowledgement_together_or_none_of_them()
-> anyhow::Result<()> {
    let prepare: PrepareTicket = Arc::new(|_, _| anyhow::bail!("No directory for the ticket"));
    let mut h = harness(Some(prepare)).await?;
    h.engine.receive(trigger("1")).await?;
    h.pump().await?;
    let req = h.last_request().await?;
    let result = h
        .decide(
            &req,
            Outcome::Accept(AcceptOutcome::new("Fix", "small", "requested").acknowledge("ok")),
        )
        .await;
    assert!(
        result
            .map_err(|e| e.to_string())
            .unwrap_err()
            .contains("No directory")
    );
    assert!(h.decisions("chat:t1").await?.is_empty());
    assert!(h.store.tickets().await?.is_empty());
    let thread = h.thread("chat:t1").await?;
    assert_eq!(thread.evaluating.map(|e| e.id), Some(req.eval_id));
    assert!(thread.subscriptions.is_empty());
    assert!(h.store.receipts_waiting().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn records_unavailable_when_the_policy_version_changed_during_assessment()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    h.engine.receive(trigger("1")).await?;
    h.pump().await?;
    h.policy.set_version("v2");
    let req = h.last_request().await?;
    let decision = h.decide(&req, accept("x")).await?;
    assert_eq!(decision.kind, DecisionKind::Unavailable);
    assert!(h.store.tickets().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn starts_every_pending_eval_in_one_step_so_a_second_pass_starts_none_again()
-> anyhow::Result<()> {
    let h = harness(None).await?;
    for (id, thread) in [("1", "A"), ("2", "B")] {
        h.engine.receive(on(trigger(id), &[chat(thread)])).await?;
    }
    h.engine.evaluate_due().await?;
    h.engine.evaluate_due().await?;
    let keys: Vec<String> = h
        .requests()
        .await?
        .into_iter()
        .map(|r| r.thread_key)
        .collect();
    assert_eq!(keys, vec!["chat:A", "chat:B"]);
    Ok(())
}

#[tokio::test]
async fn counts_an_evals_deadline_from_its_delivery_requeues_twice_when_it_expires_then_records_it_unavailable()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    h.engine.receive(trigger("1")).await?;
    h.pump().await?;
    // A candidate the PM has not taken does not expire.
    h.tick(600);
    h.pump().await?;
    let first = h.last_request().await?;
    assert_eq!(
        h.engine.candidate("1").await?.map(|c| c.eval_id),
        Some(first.eval_id.clone())
    );
    h.engine.candidate_delivered(&first.eval_id).await?;
    h.tick(59);
    h.pump().await?;
    assert_eq!(h.requests().await?.len(), 1);
    // Expired, it records nothing and queues the Trigger for a fresh Eval, twice.
    h.tick(2);
    h.pump().await?;
    assert!(h.decisions("chat:t1").await?.is_empty());
    assert!(
        h.hooks
            .released
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&first.eval_id)
    );
    let batches: Vec<Vec<String>> = h
        .requests()
        .await?
        .into_iter()
        .map(|r| r.trigger_ids)
        .collect();
    assert_eq!(batches, vec![vec!["1".to_owned()], vec!["1".to_owned()]]);
    let second = h.last_request().await?;
    h.engine.candidate_delivered(&second.eval_id).await?;
    h.tick(61);
    h.pump().await?;
    assert_eq!(h.requests().await?.len(), 3);
    // The third expiry is recorded unavailable.
    let third = h.last_request().await?;
    h.engine.candidate_delivered(&third.eval_id).await?;
    h.tick(61);
    h.pump().await?;
    let decisions = h.decisions("chat:t1").await?;
    assert_eq!(decisions.len(), 1);
    assert_eq!(
        (decisions[0].kind, decisions[0].reason[0].code.as_str()),
        (DecisionKind::Unavailable, "timeout")
    );
    let thread = h.thread("chat:t1").await?;
    assert!(thread.pending.is_empty() && thread.evaluating.is_none() && thread.expired.is_none());
    Ok(())
}

// ---- subscription fan-out ----

#[tokio::test]
async fn delivers_a_persons_later_message_on_a_subscribed_thread_to_the_live_ticket_and_not_to_eval()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let decision = h.accepted().await?;
    let requests = h.requests().await?.len();
    h.engine.receive(by(trigger("2"), guest())).await?;
    h.pump().await?;
    let ticket = h.ticket(&ticket_id(&decision)).await?;
    assert_eq!(trigger_inputs(&ticket), vec!["2"]);
    assert!(ticket.undelivered.is_empty());
    assert_eq!(
        h.delivered,
        vec![(ticket.id.clone(), ticket.inputs[0].id().to_owned())]
    );
    let disposition = h
        .store
        .disposition("2")
        .await?
        .ok_or_else(|| anyhow::anyhow!("no disposition"))?;
    assert_eq!(
        (disposition.fanout, disposition.context),
        (vec![ticket.id.clone()], None)
    );
    // No Eval, so no Decision and no timeout, and the Thread's queue stays free for a new request.
    assert_eq!(h.requests().await?.len(), requests);
    assert!(h.thread("chat:t1").await?.pending.is_empty());
    assert_eq!(h.decisions("chat:t1").await?.len(), 1);
    h.tick(600);
    h.engine.evaluate_due().await?;
    assert_eq!(h.decisions("chat:t1").await?.len(), 1);
    // A bot's message there still goes to Eval, which the gate does not admit.
    h.engine.receive(bot(trigger("3"))).await?;
    h.pump().await?;
    let last = h
        .decisions("chat:t1")
        .await?
        .pop()
        .ok_or_else(|| anyhow::anyhow!("no decision"))?;
    assert_eq!(
        (last.kind, last.trigger_ids),
        (DecisionKind::Ignore, vec!["3".to_owned()])
    );
    Ok(())
}

#[tokio::test]
async fn a_review_addressed_to_a_tickets_own_thread_reaches_its_worker_without_a_router_call()
-> anyhow::Result<()> {
    // Scenario 1, step 6: the acceptance of T-12.
    let mut h = harness(None).await?;
    let decision = h.accepted().await?;
    let id = ticket_id(&decision);
    let requests = h.requests().await?.len();
    let review = on(trigger("2"), &[thread_rules::ticket_thread(&id)]);
    h.engine.receive(review).await?;
    h.pump().await?;
    let ticket = h.ticket(&id).await?;
    assert_eq!(trigger_inputs(&ticket), vec!["2"]);
    assert_eq!(h.delivered.len(), 1);
    assert_eq!(h.requests().await?.len(), requests);
    assert!(
        h.thread(&format!("codext:ticket/{id}"))
            .await?
            .pending
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn keeps_inputs_undelivered_until_an_assignment_is_running_and_asks_to_launch_again()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    h.engine.receive(trigger("1")).await?;
    h.pump().await?;
    let req = h.last_request().await?;
    let decision = h.decide(&req, accept("Fix")).await?;
    h.settle(false).await?;
    h.engine.receive(trigger("2")).await?;
    h.engine.evaluate_due().await?;
    h.settle(false).await?;
    let ticket = h.ticket(&ticket_id(&decision)).await?;
    assert_eq!(ticket.undelivered.len(), 1);
    assert!(h.delivered.is_empty());
    // The launch is asked for again; the same assignment keeps one row.
    let launches = h.effects.queued("launch").await?;
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0].0, ticket.assignments[0].id);
    h.engine
        .assignment_started(&ticket.id, &ticket.assignments[0].id)
        .await?;
    h.cancel("launch", &ticket.assignments[0].id).await?;
    h.settle(true).await?;
    assert_eq!(
        h.delivered,
        vec![(ticket.id.clone(), ticket.undelivered[0].clone())]
    );
    Ok(())
}

#[tokio::test]
async fn does_not_hand_a_bots_message_to_an_ended_ticket_and_still_assesses_it()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let decision = h.accepted().await?;
    h.engine
        .report(
            &ticket_id(&decision),
            WorkerReport::new(ReportState::Ready, "done"),
        )
        .await?;
    h.engine.receive(bot(trigger("2"))).await?;
    h.pump().await?;
    let ticket = h.ticket(&ticket_id(&decision)).await?;
    assert!(ticket.ended.is_some() && ticket.undelivered.is_empty());
    assert_eq!(ticket.assignments.len(), 1);
    Ok(())
}

#[tokio::test]
async fn records_worker_turn_activity_and_bounds_report_text() -> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let decision = h.accepted().await?;
    let id = ticket_id(&decision);
    let assignment_id = h.ticket(&id).await?.assignments[0].id.clone();

    h.engine.turn_started(&id, &assignment_id).await?;
    let ticket = h.ticket(&id).await?;
    assert_eq!(
        ticket.last_activity.as_ref().map(|activity| activity.kind),
        Some(LastActivityKind::TurnStarted)
    );
    assert_eq!(
        ticket
            .last_activity
            .as_ref()
            .map(|activity| activity.text.as_str()),
        Some("")
    );

    h.engine.turn_ended(&id, &assignment_id).await?;
    assert_eq!(
        h.ticket(&id)
            .await?
            .last_activity
            .as_ref()
            .map(|activity| activity.kind),
        Some(LastActivityKind::TurnEnded)
    );

    h.engine
        .report(
            &id,
            WorkerReport::new(ReportState::Progress, &"字".repeat(200)),
        )
        .await?;
    let activity = h
        .ticket(&id)
        .await?
        .last_activity
        .ok_or_else(|| anyhow::anyhow!("last activity"))?;
    assert_eq!(activity.kind, LastActivityKind::Report);
    assert_eq!(activity.text.chars().count(), 120);
    Ok(())
}

#[tokio::test]
async fn resumes_once_however_often_it_pumps_and_drops_what_waited_when_the_policy_no_longer_holds_the_mandate()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let decision = h.accepted().await?;
    let id = ticket_id(&decision);
    h.engine
        .report(&id, WorkerReport::new(ReportState::Ready, "done"))
        .await?;
    h.policy.set_version("revoked");
    h.engine.receive(by(trigger("2"), guest())).await?;
    h.pump().await?;
    let ticket = h.ticket(&id).await?;
    assert!(ticket.ended.is_some() && ticket.undelivered.is_empty());
    h.policy.set_version("v1");
    h.pump().await?;
    assert!(h.ticket(&id).await?.ended.is_some());
    h.engine.receive(by(trigger("3"), guest())).await?;
    h.pump().await?;
    h.pump().await?;
    let ticket = h.ticket(&id).await?;
    assert_eq!(ticket.assignments.len(), 2);
    assert_eq!(
        ticket
            .inputs
            .iter()
            .filter(|i| matches!(i, TicketInput::Resumed { .. }))
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn takes_delivery_reports_only_from_the_assignment_that_still_runs_the_open_ticket()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let id = ticket_id(&h.accepted().await?);
    let assignment = h.ticket(&id).await?.assignments[0].id.clone();
    h.engine.receive(trigger("2")).await?;
    let input = key(["input", id.as_str(), "trigger", "2"]);
    h.engine
        .delivery_failed(&id, &assignment, "Session busy")
        .await?;
    assert_eq!(
        h.ticket(&id).await?.delivery.map(|d| d.error),
        Some("Session busy".to_owned())
    );
    h.engine.delivery_failed(&id, "another", "other").await?;
    h.engine
        .inputs_delivered(&id, "another", std::slice::from_ref(&input))
        .await?;
    let ticket = h.ticket(&id).await?;
    assert_eq!(
        (ticket.undelivered.clone(), ticket.delivery.map(|d| d.error)),
        (vec![input.clone()], Some("Session busy".to_owned()))
    );
    h.engine
        .inputs_delivered(&id, &assignment, &[input])
        .await?;
    let ticket = h.ticket(&id).await?;
    assert!(ticket.undelivered.is_empty() && ticket.delivery.is_none());
    h.engine.receive(trigger("3")).await?;
    h.engine.stop_ticket(&id, None).await?;
    h.engine.delivery_failed(&id, &assignment, "late").await?;
    let ticket = h.ticket(&id).await?;
    assert!(ticket.ended.is_some() && ticket.undelivered.is_empty() && ticket.delivery.is_none());
    Ok(())
}

#[tokio::test]
async fn unsubscribing_a_thread_stops_fan_out_until_a_new_decision_subscribes_it()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let decision = h.accepted().await?;
    assert!(h.engine.unsubscribe("chat:t1").await?);
    h.engine.receive(trigger("2")).await?;
    h.pump().await?;
    assert!(h.ticket(&ticket_id(&decision)).await?.inputs.is_empty());
    Ok(())
}

// ---- resume and stop ----

#[tokio::test]
async fn resumes_an_ended_ticket_on_a_persons_message_under_the_same_mandate_and_without_a_decision()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let id = ticket_id(&h.accepted().await?);
    h.engine
        .report(&id, WorkerReport::new(ReportState::Ready, "done"))
        .await?;
    let decisions = h.decisions("chat:t1").await?.len();
    h.engine.receive(by(trigger("2"), guest())).await?;
    assert!(h.ticket(&id).await?.ended.is_none());
    h.pump().await?;
    let ticket = h.ticket(&id).await?;
    assert_eq!(ticket.assignments.len(), 2);
    assert_eq!(ticket.mandate.policy_version, "v1");
    let kinds: Vec<&str> = ticket
        .inputs
        .iter()
        .map(|i| match i {
            TicketInput::Trigger { .. } => "trigger",
            TicketInput::Resumed { .. } => "resumed",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, vec!["trigger", "resumed"]);
    let non_ignore = h
        .decisions("chat:t1")
        .await?
        .into_iter()
        .filter(|d| d.kind != DecisionKind::Ignore)
        .count();
    assert_eq!(non_ignore, decisions);
    Ok(())
}

#[tokio::test]
async fn keeps_a_ticket_a_person_stopped_ended_the_stop_withdrew_its_mandate() -> anyhow::Result<()>
{
    let mut h = harness(None).await?;
    let id = ticket_id(&h.accepted().await?);
    h.engine.stop_ticket(&id, None).await?;
    assert_eq!(h.stops(), 1);
    h.engine.receive(trigger("2")).await?;
    h.pump().await?;
    let ticket = h.ticket(&id).await?;
    assert_eq!(ticket.ended.map(|e| e.by), Some(TicketEndBy::Human));
    assert_eq!(ticket.assignments.len(), 1);
    Ok(())
}

#[tokio::test]
async fn keeps_a_ticket_ended_when_its_mandate_no_longer_passes_the_current_policy()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let id = ticket_id(&h.accepted().await?);
    h.engine
        .report(&id, WorkerReport::new(ReportState::Ready, "done"))
        .await?;
    h.policy.set_version("revoked");
    h.engine.receive(trigger("2")).await?;
    h.pump().await?;
    assert!(h.ticket(&id).await?.ended.is_some());
    Ok(())
}

#[tokio::test]
async fn stops_a_live_ticket_only_on_an_explicit_instruction_and_ends_it_by_the_human()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let id = ticket_id(&h.accepted().await?);
    h.engine.receive(by(trigger("2"), guest())).await?;
    h.pump().await?;
    let decision = h.engine.decide("2", Outcome::stop(&id, "x"), None).await?;
    assert_eq!(
        (
            decision.kind,
            decision.strength,
            decision.reason[0].code.as_str()
        ),
        (DecisionKind::Ignore, Strength::Inferred, "stopNotExplicit")
    );
    h.engine.receive(trigger("3")).await?;
    h.pump().await?;
    assert_eq!(
        h.engine
            .decide("3", Outcome::stop(&id, "human said stop"), None)
            .await?
            .kind,
        DecisionKind::Stop
    );
    assert_eq!(
        h.ticket(&id).await?.ended.map(|e| e.by),
        Some(TicketEndBy::Human)
    );
    assert_eq!(h.stops(), 1);
    Ok(())
}

// ---- follow-ups and hand-backs ----

#[tokio::test]
async fn decides_a_follow_up_in_a_gate_only_eval_beside_the_eval_in_progress_which_goes_on()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let id = ticket_id(&h.accepted().await?);
    h.engine.receive(by(trigger("2"), guest())).await?;
    h.engine.reevaluate("2").await?;
    h.pump().await?;
    let evaluating = h.thread("chat:t1").await?.evaluating;
    h.engine.receive(trigger("3")).await?;
    let decision = h
        .engine
        .decide("3", Outcome::stop(&id, "stop it"), None)
        .await?;
    assert_eq!(
        (decision.kind, decision.trigger_ids),
        (DecisionKind::Stop, vec!["3".to_owned()])
    );
    assert_eq!(h.thread("chat:t1").await?.evaluating, evaluating);
    let req = h.last_request().await?;
    let ignored = h.decide(&req, Outcome::ignore("chat")).await?;
    assert_eq!(
        (ignored.kind, ignored.trigger_ids),
        (DecisionKind::Ignore, vec!["2".to_owned()])
    );
    Ok(())
}

#[tokio::test]
async fn queues_a_handed_back_message_for_a_fresh_eval_once_per_ticket_and_keeps_the_subscription()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let id = ticket_id(&h.accepted().await?);
    h.engine.receive(trigger("2")).await?;
    h.engine.receive(by(trigger("3"), guest())).await?;
    h.pump().await?;
    assert!(h.engine.handback(&id, "2", "A new request").await?);
    h.pump().await?;
    let req = h.last_request().await?;
    assert_eq!(
        (req.strength.as_str(), req.trigger_ids.clone()),
        ("explicit", vec!["2".to_owned()])
    );
    let handbacks = h
        .store
        .disposition("2")
        .await?
        .and_then(|d| d.handbacks)
        .unwrap_or_default();
    assert_eq!(handbacks.len(), 1);
    assert_eq!(
        (
            handbacks[0].ticket_id.as_str(),
            handbacks[0].reason.as_str()
        ),
        (id.as_str(), "A new request")
    );
    h.decide(&req, Outcome::ignore("chat")).await?;
    assert_eq!(
        code(&h.engine.handback(&id, "2", "Again").await),
        "handedBack"
    );
    assert!(h.engine.handback(&id, "3", "A guest asks for more").await?);
    h.pump().await?;
    let req = h.last_request().await?;
    assert_eq!(
        (req.strength.as_str(), req.trigger_ids.clone()),
        ("inferred", vec!["3".to_owned()])
    );
    assert!(h.thread("chat:t1").await?.subscriptions.contains(&id));
    h.engine
        .receive(on(trigger("4"), &[chat("elsewhere")]))
        .await?;
    assert_eq!(
        code(&h.engine.handback(&id, "4", "Not mine").await),
        "notReached"
    );
    Ok(())
}

#[tokio::test]
async fn queues_again_no_message_the_ticket_did_not_decide() -> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let id = ticket_id(&h.accepted().await?);
    h.engine.follow_for_context(&id, chat("issue")).await?;
    h.engine.receive(trigger("2")).await?;
    h.engine.reevaluate("2").await?;
    h.engine.receive(bot(trigger("3"))).await?;
    h.engine.receive(on(trigger("4"), &[chat("issue")])).await?;
    h.pump().await?;
    let requests = h.requests().await?;
    for req in requests.iter().rev().take(2) {
        h.decide(req, Outcome::ignore("chat")).await?;
    }
    for trigger_id in ["2", "3", "4"] {
        assert!(
            !h.engine
                .handback(&id, trigger_id, "Not this ticket")
                .await?
        );
    }
    assert!(h.thread("chat:t1").await?.pending.is_empty());
    assert!(h.thread("chat:issue").await?.pending.is_empty());
    let handbacks = h
        .store
        .disposition("4")
        .await?
        .and_then(|d| d.handbacks)
        .unwrap_or_default();
    assert_eq!(handbacks.len(), 1);
    assert_eq!(
        code(&h.engine.attach(&id, "4", None).await),
        "handedBackHere"
    );
    Ok(())
}

#[tokio::test]
async fn leaves_a_message_the_pm_attached_to_its_worker_whose_hand_back_sends_it_to_eval()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let id = ticket_id(&h.accepted().await?);
    h.engine.receive(on(trigger("2"), &[chat("x")])).await?;
    h.pump().await?;
    h.engine.attach(&id, "2", None).await?;
    h.settle(true).await?;
    assert_eq!(
        code(&h.engine.decide("2", accept("Y"), None).await),
        "workerDecides"
    );
    assert!(
        h.engine
            .handback(&id, "2", "Asks for another change")
            .await?
    );
    assert_eq!(h.thread("chat:x").await?.pending, vec!["2"]);
    Ok(())
}

// ---- context threads and notes ----

#[tokio::test]
async fn fans_a_thread_followed_for_context_out_to_the_live_ticket_and_resumes_no_ended_ticket_from_it_unless_attached()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let id = ticket_id(&h.accepted().await?);
    assert!(h.engine.follow_for_context(&id, chat("issue")).await?);
    assert!(!h.engine.follow_for_context(&id, chat("issue")).await?);
    h.engine.receive(on(trigger("2"), &[chat("issue")])).await?;
    h.settle(true).await?;
    assert_eq!(trigger_inputs(&h.ticket(&id).await?), vec!["2"]);
    h.engine
        .report(
            &id,
            WorkerReport::new(ReportState::Dropped, "nothing to do"),
        )
        .await?;
    h.engine.receive(on(trigger("3"), &[chat("issue")])).await?;
    assert!(h.ticket(&id).await?.ended.is_some());
    // Attached by the PM, the Thread is the ticket's own, and a person's later message there resumes it.
    h.engine.attach(&id, "3", None).await?;
    h.settle(true).await?;
    assert_eq!(h.thread("chat:issue").await?.context, Some(vec![]));
    h.engine
        .report(
            &id,
            WorkerReport::new(ReportState::Dropped, "nothing to do"),
        )
        .await?;
    assert!(h.ticket(&id).await?.ended.is_some());
    h.engine.receive(on(trigger("4"), &[chat("issue")])).await?;
    assert!(h.ticket(&id).await?.ended.is_none());
    Ok(())
}

#[tokio::test]
async fn gives_an_open_tickets_worker_a_note_held_like_automatic_input_and_refuses_one_for_an_ended_ticket()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let id = ticket_id(&h.accepted().await?);
    let noted = h
        .engine
        .note(&id, "The other ticket renamed the API")
        .await?;
    assert!(
        matches!(noted.inputs.last(), Some(TicketInput::Note { text, .. }) if text == "The other ticket renamed the API")
    );
    h.settle(true).await?;
    h.engine
        .report(
            &id,
            WorkerReport::new(ReportState::Blocked, "needs a decision"),
        )
        .await?;
    let note = h
        .engine
        .note(&id, "Ticket B changed the schema")
        .await?
        .inputs
        .last()
        .map(|i| i.id().to_owned())
        .unwrap_or_default();
    h.settle(true).await?;
    assert!(!h.delivered.iter().any(|(_, input)| *input == note));
    assert!(input_ids(&h.ticket(&id).await?).contains(&note));
    h.engine.stop_ticket(&id, Some("done")).await?;
    assert_eq!(code(&h.engine.note(&id, "Too late").await), "ticketEnded");
    Ok(())
}

// ---- attach and direct decisions ----

#[tokio::test]
async fn attach_sends_the_routing_note_ends_an_eval_holding_only_that_message_and_refuses_stopped_or_revoked_tickets()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let id = ticket_id(&h.accepted().await?);
    h.engine.receive(by(trigger("2"), guest())).await?;
    h.pump().await?;
    assert_eq!(
        h.delivered
            .iter()
            .map(|(_, i)| i.clone())
            .collect::<Vec<_>>(),
        vec![key(["input", id.as_str(), "trigger", "2"])]
    );
    assert!(h.engine.candidate("2").await?.is_none());
    assert_eq!(
        code(&h.engine.attach(&id, "2", None).await),
        "workerDecides"
    );
    assert!(h.engine.reevaluate("2").await?);
    h.pump().await?;
    assert!(h.engine.candidate("2").await?.is_some());
    let attached = h
        .engine
        .attach(&id, "2", Some("The guest asks for the same fix"))
        .await?;
    h.settle(true).await?;
    assert_eq!(trigger_inputs(&attached), vec!["2", "2"]);
    assert!(
        matches!(&attached.inputs[1], TicketInput::Trigger { note: Some(n), .. } if n == "The guest asks for the same fix")
    );
    assert_eq!(
        h.delivered
            .iter()
            .map(|(_, i)| i.clone())
            .collect::<Vec<_>>(),
        input_ids(&attached)
    );
    assert!(h.engine.candidate("2").await?.is_none());
    let last = h
        .decisions("chat:t1")
        .await?
        .pop()
        .ok_or_else(|| anyhow::anyhow!("no decision"))?;
    assert_eq!(
        (last.kind, last.reason[0].code.as_str()),
        (DecisionKind::Ignore, "routed")
    );
    h.engine.receive(by(trigger("5"), guest())).await?;
    h.engine.reevaluate("5").await?;
    let again = h.engine.attach(&id, "5", Some("Same fix again")).await?;
    assert!(
        matches!(again.inputs.last(), Some(TicketInput::Trigger { trigger_id, note: Some(n), .. }) if trigger_id == "5" && n == "Same fix again")
    );
    h.settle(true).await?;
    // The report answers "2" (taken up); "5" was never taken up, so the end of the ticket hands it
    // back (engine rule of 11/03).
    let mut done = WorkerReport::new(ReportState::Ready, "done");
    done.answers = Some("2".into());
    h.engine.report(&id, done).await?;
    assert!(
        h.store
            .disposition("5")
            .await?
            .and_then(|d| d.handbacks)
            .is_some_and(|h| h.iter().any(|item| item.ticket_id == id))
    );
    h.policy.set_version("revoked");
    assert_eq!(
        code(&h.engine.attach(&id, "2", None).await),
        "mandateRevoked"
    );
    h.policy.set_version("v1");
    let stopped = ticket_id(&h.accepted_on("3", "t3").await?);
    h.engine.stop_ticket(&stopped, None).await?;
    assert_eq!(
        code(&h.engine.attach(&stopped, "2", None).await),
        "ticketStopped"
    );
    assert_eq!(
        code(&h.engine.attach("missing", "2", None).await),
        "unknownTicket"
    );
    Ok(())
}

#[tokio::test]
async fn decides_an_explicit_request_no_eval_holds_directly_refuses_a_proactive_accept_without_rules_and_accepts_once()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    h.engine.receive(trigger("1")).await?;
    let decision = h.engine.decide("1", accept("Fix"), None).await?;
    assert_eq!(
        (
            decision.kind,
            decision.trigger_ids.clone(),
            decision.strength
        ),
        (
            DecisionKind::Accept,
            vec!["1".to_owned()],
            Strength::Explicit
        )
    );
    let thread = h.thread("chat:t1").await?;
    assert!(thread.pending.is_empty() && thread.evaluating.is_none());
    assert_eq!(h.engine.decide("1", accept("Again"), None).await?, decision);
    h.settle(true).await?;
    h.engine.receive(by(trigger("2"), guest())).await?;
    h.pump().await?;
    let id = ticket_id(&decision);
    assert_eq!(
        code(
            &h.engine
                .decide(
                    "2",
                    Outcome::Accept(AcceptOutcome::new("Fix", "small", "rule").rules(&["r1"])),
                    None
                )
                .await
        ),
        "workerDecides"
    );
    h.engine.handback(&id, "2", "A different fix").await?;
    assert_eq!(
        code(&h.engine.decide("2", accept("Fix"), None).await),
        "notExplicit"
    );
    let inferred = h
        .engine
        .decide(
            "2",
            Outcome::Accept(AcceptOutcome::new("Fix", "small", "rule").rules(&["r1"])),
            None,
        )
        .await?;
    assert_eq!(
        (inferred.kind, inferred.strength),
        (DecisionKind::Accept, Strength::Inferred)
    );
    assert_eq!(
        code(&h.engine.decide("missing", Outcome::ignore("x"), None).await),
        "unknownTrigger"
    );
    Ok(())
}

#[tokio::test]
async fn ends_the_eval_of_a_replayed_trigger_that_started_a_ticket_and_returns_that_tickets_decision()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let decision = h.accepted().await?;
    h.tick(1);
    assert!(h.engine.reevaluate("1").await?);
    h.pump().await?;
    assert!(h.engine.candidate("1").await?.is_none());
    let last = h
        .decisions("chat:t1")
        .await?
        .pop()
        .ok_or_else(|| anyhow::anyhow!("no decision"))?;
    assert_eq!(
        (
            last.kind,
            last.trigger_ids.clone(),
            last.reason[0].code.as_str()
        ),
        (DecisionKind::Ignore, vec!["1".to_owned()], "startedTicket")
    );
    assert_eq!(
        last.reason[0].values.as_ref().and_then(|v| v.get("ticket")),
        decision.ticket_id.as_ref()
    );
    assert_eq!(h.engine.decide("1", accept("Fix"), None).await?, decision);
    assert_eq!(h.store.tickets().await?.len(), 1);
    assert_eq!(
        code(&h.engine.reevaluate("missing").await),
        "unknownTrigger"
    );
    Ok(())
}

// ---- automatic input, pause, policy change ----

#[tokio::test]
async fn holds_back_automatic_input_from_a_worker_that_reported_blocked_until_a_persons_message_arrives()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let id = ticket_id(&h.accepted().await?);
    h.engine.receive(bot(trigger("2"))).await?;
    h.engine
        .report(
            &id,
            WorkerReport::new(ReportState::Blocked, "needs a decision"),
        )
        .await?;
    h.engine.receive(bot(trigger("3"))).await?;
    h.settle(true).await?;
    let ticket = h.ticket(&id).await?;
    assert!(ticket.undelivered.is_empty());
    assert_eq!(trigger_inputs(&ticket), vec!["2", "3"]);
    assert!(h.delivered.is_empty());
    h.tick(1);
    h.engine.receive(by(trigger("4"), guest())).await?;
    h.engine.receive(bot(trigger("5"))).await?;
    h.settle(true).await?;
    let delivered: Vec<String> = h.delivered.iter().map(|(_, i)| i.clone()).collect();
    assert_eq!(
        delivered,
        vec![
            key(["input", id.as_str(), "trigger", "4"]),
            key(["input", id.as_str(), "trigger", "5"])
        ]
    );
    Ok(())
}

#[tokio::test]
async fn stops_waiting_for_a_person_on_a_persons_message_a_later_report_or_a_retry()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let id = ticket_id(&h.accepted().await?);
    h.engine
        .report(
            &id,
            WorkerReport::new(ReportState::Blocked, "needs a decision"),
        )
        .await?;
    assert!(waits_for_person(&h.ticket(&id).await?));
    h.tick(1);
    h.engine.receive(by(trigger("2"), guest())).await?;
    h.settle(true).await?;
    assert!(!waits_for_person(&h.ticket(&id).await?));
    h.tick(1);
    h.engine
        .report(&id, WorkerReport::new(ReportState::Failed, "broke"))
        .await?;
    assert!(waits_for_person(&h.ticket(&id).await?));
    h.tick(1);
    h.engine
        .report(&id, WorkerReport::new(ReportState::Progress, "found a way"))
        .await?;
    assert!(!waits_for_person(&h.ticket(&id).await?));
    h.tick(1);
    h.engine
        .report(&id, WorkerReport::new(ReportState::Blocked, "needs access"))
        .await?;
    h.engine.retry_ticket(&id).await?;
    assert!(!waits_for_person(&h.ticket(&id).await?));
    assert_eq!(h.stops(), 1);
    Ok(())
}

#[tokio::test]
async fn lets_no_eval_expire_while_paused_and_moves_its_deadline_on_by_the_pauses_length()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    h.engine.receive(trigger("1")).await?;
    h.pump().await?;
    let first = h.last_request().await?;
    h.engine.candidate_delivered(&first.eval_id).await?;
    let deadline = h
        .thread("chat:t1")
        .await?
        .evaluating
        .map(|e| e.deadline)
        .unwrap_or_default();
    h.tick(30);
    h.policy.set_paused(true);
    h.pump().await?;
    h.tick(300);
    h.pump().await?;
    assert!(h.engine.candidate("1").await?.is_some());
    h.policy.set_paused(false);
    h.pump().await?;
    let shifted = h
        .thread("chat:t1")
        .await?
        .evaluating
        .map(|e| e.deadline)
        .unwrap_or_default();
    assert_eq!(
        shifted,
        codex_superagent_extension::core::evals::add_ms(&deadline, 300_000)
    );
    h.tick(29);
    h.pump().await?;
    assert!(h.engine.candidate("1").await?.is_some());
    h.tick(1);
    h.pump().await?;
    assert_ne!(
        h.engine.candidate("1").await?.map(|c| c.eval_id),
        Some(first.eval_id)
    );
    assert!(h.decisions("chat:t1").await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn tells_only_the_live_tickets_a_policy_change_bears_on_and_restarts_in_flight_evals_under_the_new_version()
-> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let decision = h.accepted().await?;
    let other = ticket_id(&h.accepted_on("3", "t3").await?);
    h.engine.receive(on(trigger("2"), &[chat("t2")])).await?;
    h.pump().await?;
    let effect = PolicyEffect {
        rules: Some(vec![RuleEffect {
            id: "copy".into(),
            name: "Copy".into(),
            change: RuleChange::Changed,
            instruction: Some("Fix typos only.".into()),
        }]),
        ..PolicyEffect::default()
    };
    h.policy.set_version("v2");
    let id = ticket_id(&decision);
    h.engine
        .policy_changed("v1", "v2", HashMap::from([(id.clone(), effect.clone())]))
        .await?;
    let told = h.ticket(&id).await?;
    assert!(
        matches!(told.inputs.last(), Some(TicketInput::PolicyChanged { from, to, effect: Some(e), .. }) if from == "v1" && to == "v2" && *e == effect)
    );
    assert!(
        !h.ticket(&other)
            .await?
            .inputs
            .iter()
            .any(|i| matches!(i, TicketInput::PolicyChanged { .. }))
    );
    let decisions = h.decisions("chat:t2").await?;
    assert_eq!(decisions.len(), 1);
    assert_eq!(
        (decisions[0].kind, decisions[0].reason[0].code.as_str()),
        (DecisionKind::Unavailable, "policyChanged")
    );
    assert_eq!(h.thread("chat:t2").await?.pending, vec!["2"]);
    h.pump().await?;
    let req = h.last_request().await?;
    assert_eq!(req.trigger_ids, vec!["2".to_owned()]);
    assert_eq!(
        h.engine.candidate("2").await?.map(|c| c.policy_version),
        Some("v2".to_owned())
    );
    let told_id = told
        .inputs
        .last()
        .map(|i| i.id().to_owned())
        .unwrap_or_default();
    assert_eq!(h.delivered, vec![(id, told_id)]);
    Ok(())
}

#[tokio::test]
async fn does_not_start_evals_while_paused() -> anyhow::Result<()> {
    let mut h = harness(None).await?;
    h.policy.set_paused(true);
    h.engine.receive(trigger("1")).await?;
    h.pump().await?;
    assert!(h.requests().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn emits_domain_events_once_a_command_commits() -> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let mut events = h.engine.subscribe();
    h.accepted().await?;
    let mut types = Vec::new();
    while let Ok(event) = events.try_recv() {
        types.push(
            serde_json::to_value(&event)?["type"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        );
    }
    assert!(types.contains(&"trigger.received".to_owned()));
    assert!(types.contains(&"eval.started".to_owned()));
    assert!(types.contains(&"decision.recorded".to_owned()));
    assert!(types.contains(&"ticket.created".to_owned()));
    assert!(types.contains(&"assignment.started".to_owned()));
    let _ = json!({});
    Ok(())
}

#[tokio::test]
async fn a_tickets_own_thread_reaches_that_ticket_alone_even_after_a_handback_opened_another()
-> anyhow::Result<()> {
    // Seen live 11/03: a message naming t005 was handed back and accepted as t014; t014 then
    // followed ticket/t005, and every later message naming t005 resumed t014 as well.
    let mut h = harness(None).await?;
    let a = ticket_id(&h.accepted().await?);
    h.engine
        .receive(on(trigger("2"), &[thread_rules::ticket_thread(&a)]))
        .await?;
    h.pump().await?;
    assert!(h.engine.handback(&a, "2", "Not this ticket's scope").await?);
    h.pump().await?;
    let req = h.last_request().await?;
    assert_eq!(req.trigger_ids, vec!["2".to_owned()]);
    let b = ticket_id(&h.decide(&req, accept("Other")).await?);
    h.settle(false).await?;
    assert_eq!(
        h.thread(&format!("codext:ticket/{a}")).await?.subscriptions,
        vec![a.clone()],
        "the new ticket does not follow the named ticket's own Thread"
    );
    h.engine
        .receive(on(trigger("3"), &[thread_rules::ticket_thread(&a)]))
        .await?;
    h.pump().await?;
    let fanout = h
        .store
        .disposition("3")
        .await?
        .map(|d| d.fanout)
        .unwrap_or_default();
    assert_eq!(fanout, vec![a.clone()]);
    assert!(
        !trigger_inputs(&h.ticket(&b).await?)
            .iter()
            .any(|id| AsRef::<str>::as_ref(id) == "3")
    );
    Ok(())
}

#[tokio::test]
async fn a_follow_up_the_worker_neither_took_up_nor_handed_back_goes_back_when_the_ticket_ends()
-> anyhow::Result<()> {
    // 11/03: a worker on its fifth assignment reported "已退回" and called no tool.
    let mut h = harness(None).await?;
    let a = ticket_id(&h.accepted().await?);
    h.engine
        .receive(on(trigger("2"), &[thread_rules::ticket_thread(&a)]))
        .await?;
    h.pump().await?;
    assert_eq!(h.delivered.len(), 1);
    let requests = h.requests().await?.len();
    h.engine
        .report(&a, WorkerReport::new(ReportState::Ready, "done"))
        .await?;
    h.pump().await?;
    let handbacks = h
        .store
        .disposition("2")
        .await?
        .and_then(|d| d.handbacks)
        .unwrap_or_default();
    assert_eq!(handbacks.len(), 1, "{handbacks:?}");
    assert_eq!(handbacks[0].ticket_id, a);
    assert!(handbacks[0].reason.contains("没有处理"));
    assert_eq!(h.requests().await?.len(), requests + 1);
    assert_eq!(h.last_request().await?.trigger_ids, vec!["2".to_owned()]);
    Ok(())
}

#[tokio::test]
async fn a_follow_up_the_final_report_answers_is_taken_up_and_stays() -> anyhow::Result<()> {
    let mut h = harness(None).await?;
    let a = ticket_id(&h.accepted().await?);
    h.engine
        .receive(on(trigger("2"), &[thread_rules::ticket_thread(&a)]))
        .await?;
    h.pump().await?;
    let requests = h.requests().await?.len();
    let mut answered = WorkerReport::new(ReportState::Ready, "done");
    answered.answers = Some("2".into());
    h.engine.report(&a, answered).await?;
    h.pump().await?;
    assert!(
        h.store
            .disposition("2")
            .await?
            .and_then(|d| d.handbacks)
            .unwrap_or_default()
            .is_empty()
    );
    assert_eq!(h.requests().await?.len(), requests);
    Ok(())
}
