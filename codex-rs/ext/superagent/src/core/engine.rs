//! The engine: the single writer of Triggers, Threads, Decisions, Tickets and Assignments, and
//! the role views its callers receive (prototype `src/core/engine.ts` and `part.ts`).
//!
//! Every command runs in one transaction; the effects it asks for (launch, delivery, assessment,
//! reply) are rows written in that transaction, and the in-memory follow-ups (`EngineHooks`) run
//! after it commits, as do the domain events.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::PoisonError;
use std::sync::RwLock;

use serde_json::json;
use tokio::sync::Semaphore;
use tokio::sync::broadcast;

use super::assignments;
use super::errors::UserError;
use super::evals;
use super::intake;
use super::model::Assignment;
use super::model::CloseOutcome;
use super::model::Decision;
use super::model::FollowupRecords;
use super::model::PolicyEffect;
use super::model::Thread;
use super::model::ThreadRef;
use super::model::Ticket;
use super::model::Trigger;
use super::model::TriggerDisposition;
use super::model::followup_owners;
use super::model::live_ticket;
use super::model::now_iso;
use super::model::reached;
use super::model::thread_key;
use super::ports::AcceptOutcome;
use super::ports::Candidate;
use super::ports::DomainEvent;
use super::ports::EngineHooks;
use super::ports::Event;
use super::ports::Outcome;
use super::ports::Policy;
use super::ports::ReceiveResult;
use super::ports::Reply;
use super::ports::ReplyRequest;
use super::ports::SourceScope;
use super::ports::WorkerReport;
use super::replies;
use super::tickets;
use crate::BoxFut;
use crate::effects::Effects;
use crate::store::OutboundMessage;
use crate::store::ReplyStatus;
use crate::store::Store;
use crate::store::Tx;

pub type Clock = Arc<dyn Fn() -> String + Send + Sync>;
/// What the executor adds to a new ticket: its directory and assignment text (prototype `prepareTicket`).
pub type PrepareTicket =
    Arc<dyn Fn(Ticket, &AcceptOutcome) -> anyhow::Result<Ticket> + Send + Sync>;

pub struct EngineOptions {
    pub store: Store,
    pub effects: Effects,
    pub policy: Arc<dyn Policy>,
    pub scope: Arc<dyn SourceScope>,
    pub hooks: Arc<dyn EngineHooks>,
    pub now: Option<Clock>,
    pub prepare_ticket: Option<PrepareTicket>,
    /// The PM whose tickets this engine numbers (`tNNN`).
    pub pm: String,
}

pub(crate) struct Inner {
    pub(crate) store: Store,
    pub(crate) effects: Effects,
    policy: RwLock<Arc<dyn Policy>>,
    pub(crate) scope: Arc<dyn SourceScope>,
    hooks: RwLock<Arc<dyn EngineHooks>>,
    now: Clock,
    prepare_ticket: RwLock<Option<PrepareTicket>>,
    pub(crate) pm: String,
    writer: Arc<Semaphore>,
    events: broadcast::Sender<Event>,
}

impl Inner {
    pub(crate) fn hooks(&self) -> Arc<dyn EngineHooks> {
        Arc::clone(&self.hooks.read().unwrap_or_else(PoisonError::into_inner))
    }

    pub(crate) fn prepare_ticket(&self) -> Option<PrepareTicket> {
        self.prepare_ticket
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

/// One command's context: its transaction, the moment it runs at, the Policy it runs under and
/// the events it emits once it commits.
pub(crate) struct Cx<'a> {
    pub(crate) e: &'a Inner,
    pub(crate) tx: &'a mut Tx,
    pub(crate) now: String,
    pub(crate) policy: Arc<dyn Policy>,
    pub(crate) events: Vec<Event>,
}

/// A refusal a person or an agent can cause, as an error the tools word by its code.
pub fn refuse(code: &str, values: &[(&str, &str)]) -> anyhow::Error {
    anyhow::Error::from(UserError::new(code, values))
}

/// The refusal in an error, if it is one.
pub fn refusal(err: &anyhow::Error) -> Option<&UserError> {
    err.downcast_ref::<UserError>()
}

struct Loaded {
    tickets: HashMap<String, Ticket>,
    threads: HashMap<String, Thread>,
    disposition: Option<TriggerDisposition>,
}

impl FollowupRecords for Loaded {
    fn ticket(&self, id: &str) -> Option<Ticket> {
        self.tickets.get(id).cloned()
    }

    fn thread(&self, key: &str) -> Option<Thread> {
        self.threads.get(key).cloned()
    }

    fn disposition(&self, _trigger_id: &str) -> Option<TriggerDisposition> {
        self.disposition.clone()
    }
}

impl Cx<'_> {
    pub(crate) fn emit(&mut self, event: DomainEvent) {
        self.events.push(Event {
            at: self.now.clone(),
            event,
        });
    }

    pub(crate) async fn require(&mut self, ticket_id: &str) -> anyhow::Result<Ticket> {
        self.tx
            .ticket(ticket_id)
            .await?
            .ok_or_else(|| refuse("unknownTicket", &[]))
    }

    pub(crate) async fn require_trigger(&mut self, trigger_id: &str) -> anyhow::Result<Trigger> {
        self.tx
            .trigger(trigger_id)
            .await?
            .ok_or_else(|| refuse("unknownTrigger", &[]))
    }

    /// The stored Thread, or an unsaved empty one.
    pub(crate) async fn thread(&mut self, reference: &ThreadRef) -> anyhow::Result<Thread> {
        let key = thread_key(reference);
        Ok(self
            .tx
            .thread(&key)
            .await?
            .unwrap_or_else(|| Thread::new(reference.clone(), &self.now)))
    }

    pub(crate) async fn tickets_on_thread(
        &mut self,
        thread: &Thread,
    ) -> anyhow::Result<Vec<Ticket>> {
        let mut tickets = Vec::new();
        for id in &thread.subscriptions {
            if let Some(ticket) = self.tx.ticket(id).await? {
                tickets.push(ticket);
            }
        }
        Ok(tickets)
    }

    /// The tickets whose worker decides a person's follow-up (`followup_owners`).
    pub(crate) async fn owners(&mut self, trigger: &Trigger) -> anyhow::Result<Vec<String>> {
        if !trigger.human {
            return Ok(Vec::new());
        }
        let disposition = self.tx.disposition(&trigger.id).await?;
        let mut tickets = HashMap::new();
        let mut threads = HashMap::new();
        if let Some(disposition) = &disposition {
            for id in &disposition.fanout {
                if let Some(ticket) = self.tx.ticket(id).await? {
                    tickets.insert(id.clone(), ticket);
                }
            }
        }
        if let Some(primary) = trigger.threads.first() {
            let key = thread_key(primary);
            if let Some(thread) = self.tx.thread(&key).await? {
                threads.insert(key, thread);
            }
        }
        Ok(followup_owners(
            trigger,
            &Loaded {
                tickets,
                threads,
                disposition,
            },
        ))
    }

    /// Refuses a PM routing of a follow-up a ticket's worker still decides.
    pub(crate) async fn left_to_workers(&mut self, trigger: &Trigger) -> anyhow::Result<()> {
        let owners = self.owners(trigger).await?;
        if !owners.is_empty() {
            return Err(refuse("workerDecides", &[("tickets", &owners.join(", "))]));
        }
        Ok(())
    }

    /// Records that a ticket's worker hands a Trigger back, with why, and queues it for Eval when
    /// the ticket decided it and no other ticket still does. Answers whether it was queued.
    pub(crate) async fn hand_back(
        &mut self,
        ticket_id: &str,
        trigger: &Trigger,
        reason: &str,
    ) -> anyhow::Result<bool> {
        let decided = self.owners(trigger).await?.iter().any(|id| id == ticket_id);
        let mut disposition =
            self.tx
                .disposition(&trigger.id)
                .await?
                .unwrap_or_else(|| TriggerDisposition {
                    at: self.now.clone(),
                    ..TriggerDisposition::default()
                });
        disposition
            .handbacks
            .get_or_insert_with(Vec::new)
            .push(super::model::Handback {
                ticket_id: ticket_id.to_owned(),
                reason: reason.to_owned(),
                at: self.now.clone(),
            });
        self.tx.save_disposition(&trigger.id, &disposition).await?;
        if !decided || !self.owners(trigger).await?.is_empty() {
            return Ok(false);
        }
        self.queue_for_eval(trigger).await
    }

    /// Records that a ticket's worker took a person's message up, by acknowledging or answering it.
    pub(crate) async fn take_up(
        &mut self,
        ticket_id: &str,
        trigger_id: &str,
    ) -> anyhow::Result<()> {
        if let Some(mut disposition) = self.tx.disposition(trigger_id).await? {
            let acked = disposition.acked.get_or_insert_with(Vec::new);
            if !acked.iter().any(|id| id == ticket_id) {
                acked.push(ticket_id.to_owned());
                self.tx.save_disposition(trigger_id, &disposition).await?;
            }
        }
        Ok(())
    }

    /// Queues a Trigger on its primary Thread for the next Eval, unless it waits there or an Eval holds it.
    pub(crate) async fn queue_for_eval(&mut self, trigger: &Trigger) -> anyhow::Result<bool> {
        let Some(reference) = trigger.threads.first() else {
            return Ok(false);
        };
        let mut primary = self.thread(reference).await?;
        let held = primary
            .evaluating
            .as_ref()
            .is_some_and(|evaluation| evaluation.trigger_ids.contains(&trigger.id));
        if primary.pending.contains(&trigger.id) || held {
            return Ok(false);
        }
        primary.pending.push(trigger.id.clone());
        self.tx.save_thread(&primary).await?;
        Ok(true)
    }

    pub(crate) async fn source(&mut self, ticket: &Ticket) -> anyhow::Result<Option<Trigger>> {
        self.tx.trigger(&ticket.origin.trigger_id).await
    }

    /// Whether a Trigger reached a ticket (`reached`), with `context` false only on its own Threads.
    pub(crate) async fn reaches(
        &mut self,
        ticket: &Ticket,
        trigger: &Trigger,
        context: bool,
    ) -> anyhow::Result<bool> {
        let mut threads = HashMap::new();
        for reference in &trigger.threads {
            let key = thread_key(reference);
            if let Some(thread) = self.tx.thread(&key).await? {
                threads.insert(key, thread);
            }
        }
        Ok(reached(
            ticket,
            trigger,
            |key| threads.get(key).cloned(),
            context,
        ))
    }

    // ---- the executor port: durable launches and deliveries, and the stop that runs after commit ----

    pub(crate) async fn executor_start(
        &mut self,
        ticket: &Ticket,
        assignment: &Assignment,
    ) -> anyhow::Result<()> {
        self.e
            .effects
            .enqueue(
                self.tx,
                "launch",
                &assignment.id,
                &json!({ "ticketId": ticket.id, "assignmentId": assignment.id }),
            )
            .await
    }

    pub(crate) async fn executor_deliver(&mut self, ticket: &Ticket) -> anyhow::Result<()> {
        self.e
            .effects
            .enqueue(
                self.tx,
                "delivery",
                &ticket.id,
                &json!({ "ticketId": ticket.id }),
            )
            .await
    }

    pub(crate) fn executor_stop(&mut self, ticket: &Ticket, assignment: &Assignment, reason: &str) {
        let hooks = self.e.hooks();
        let ticket = ticket.clone();
        let assignment = assignment.clone();
        let reason = reason.to_owned();
        self.tx
            .after_commit(move || hooks.assignment_stopped(&ticket, &assignment, &reason));
    }

    // ---- the assessor port: a durable request, and the release that runs after commit ----

    pub(crate) async fn assessor_request(&mut self, candidate: &Candidate) -> anyhow::Result<()> {
        let trigger_id = candidate
            .triggers
            .first()
            .map(|t| t.id.clone())
            .unwrap_or_default();
        self.e
            .effects
            .enqueue(
                self.tx,
                "assessment",
                &candidate.eval_id,
                &json!({
                    "triggerId": trigger_id,
                    "evalId": candidate.eval_id,
                    "threadKey": candidate.thread.key,
                    "triggerIds": candidate.triggers.iter().map(|t| t.id.clone()).collect::<Vec<_>>(),
                    "strength": candidate.strength,
                }),
            )
            .await
    }

    pub(crate) fn assessor_release(&mut self, eval_id: &str) {
        let hooks = self.e.hooks();
        let eval_id = eval_id.to_owned();
        self.tx.after_commit(move || hooks.eval_released(&eval_id));
    }

    // ---- the outbox port: receipts and the reply effect ----

    pub(crate) async fn outbox_send(&mut self, message: OutboundMessage) -> anyhow::Result<()> {
        if self.tx.receipt_add(&message).await? {
            self.e
                .effects
                .enqueue(self.tx, "reply", &message.id, &json!({}))
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn outbox_status(&mut self, id: &str) -> anyhow::Result<Option<ReplyStatus>> {
        Ok(self.tx.receipt(id).await?.map(|receipt| receipt.status))
    }

    pub(crate) async fn outbox_withdraw(&mut self, id: &str) -> anyhow::Result<()> {
        if self.tx.receipt_withdraw(id).await? {
            self.e.effects.cancel(self.tx, "reply", id).await?;
        }
        Ok(())
    }
}

impl Engine {
    pub fn new(options: EngineOptions) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(Inner {
                store: options.store,
                effects: options.effects,
                policy: RwLock::new(options.policy),
                scope: options.scope,
                hooks: RwLock::new(options.hooks),
                now: options.now.unwrap_or_else(|| Arc::new(now_iso)),
                prepare_ticket: RwLock::new(options.prepare_ticket),
                pm: options.pm,
                writer: Arc::new(Semaphore::new(1)),
                events,
            }),
        }
    }

    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    pub fn effects(&self) -> &Effects {
        &self.inner.effects
    }

    pub fn policy(&self) -> Arc<dyn Policy> {
        Arc::clone(
            &self
                .inner
                .policy
                .read()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// Installs a new Policy snapshot; tell the engine with `policy_changed` when its version changed.
    pub fn set_policy(&self, policy: Arc<dyn Policy>) {
        *self
            .inner
            .policy
            .write()
            .unwrap_or_else(PoisonError::into_inner) = policy;
    }

    pub fn set_hooks(&self, hooks: Arc<dyn EngineHooks>) {
        *self
            .inner
            .hooks
            .write()
            .unwrap_or_else(PoisonError::into_inner) = hooks;
    }

    pub fn set_prepare_ticket(&self, prepare: Option<PrepareTicket>) {
        *self
            .inner
            .prepare_ticket
            .write()
            .unwrap_or_else(PoisonError::into_inner) = prepare;
    }

    /// The domain events, from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// Runs one command as the single writer, in one transaction; its events go out once it commits.
    async fn command<T>(
        &self,
        work: impl AsyncFnOnce(&mut Cx<'_>) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let permit = Arc::clone(&self.inner.writer).acquire_owned().await?;
        let now = (self.inner.now)();
        let policy = self.policy();
        let inner = Arc::clone(&self.inner);
        let (value, events) = inner
            .store
            .atomic(async |tx| {
                let mut cx = Cx {
                    e: inner.as_ref(),
                    tx,
                    now: now.clone(),
                    policy: Arc::clone(&policy),
                    events: Vec::new(),
                };
                let value = work(&mut cx).await?;
                Ok((value, std::mem::take(&mut cx.events)))
            })
            .await?;
        drop(permit);
        for event in events {
            let _ = self.inner.events.send(event);
        }
        Ok(value)
    }

    // ---- sources ----

    /// A Source's request to take in a Trigger: stored once, fanned out to the tickets subscribed to
    /// its Threads, and queued for Eval on its primary Thread unless a ticket's worker decides it.
    pub async fn receive(&self, trigger: Trigger) -> anyhow::Result<ReceiveResult> {
        self.command(async |cx| intake::receive(cx, trigger).await)
            .await
    }

    /// Makes an open ticket follow one of a Source's Threads, `own` when the ticket opened it.
    pub async fn bind_thread(
        &self,
        ticket_id: &str,
        thread: ThreadRef,
        own: bool,
    ) -> anyhow::Result<Ticket> {
        self.command(async |cx| tickets::bind(cx, ticket_id, &thread, own).await)
            .await
    }

    /// A Thread open tickets follow closed, such as a pull request's merge.
    pub async fn thread_closed(
        &self,
        thread: ThreadRef,
        outcome: CloseOutcome,
        at: &str,
    ) -> anyhow::Result<()> {
        let at = super::model::iso_time(at);
        self.command(async |cx| tickets::thread_closed(cx, &thread, outcome, &at).await)
            .await
    }

    // ---- the clock ----

    /// Expires overdue Evals, resumes ended tickets a person's message reached while processing was
    /// paused, and starts the Evals of Threads whose queued Triggers wait. Each in its own transaction.
    pub async fn evaluate_due(&self) -> anyhow::Result<()> {
        let policy = self.policy();
        for thread in self.inner.store.threads_active().await? {
            if thread.evaluating.is_some() {
                let key = thread.key.clone();
                self.command(async |cx| evals::due(cx, &key).await).await?;
            }
        }
        if policy.paused() {
            return Ok(());
        }
        for ticket in self.inner.store.tickets_active().await? {
            if !live_ticket(&ticket) && !ticket.undelivered.is_empty() {
                let id = ticket.id.clone();
                self.command(async |cx| tickets::wake(cx, &id).await)
                    .await?;
            }
        }
        for thread in self.inner.store.threads_active().await? {
            if !thread.pending.is_empty() && thread.evaluating.is_none() {
                let key = thread.key.clone();
                self.command(async |cx| evals::start(cx, &key).await)
                    .await?;
            }
        }
        Ok(())
    }

    // ---- the PM ----

    /// The candidate of the Eval that holds a Trigger; none while no Eval holds it.
    pub async fn candidate(&self, trigger_id: &str) -> anyhow::Result<Option<Candidate>> {
        self.command(async |cx| evals::candidate(cx, trigger_id).await)
            .await
    }

    /// The assessor delivered an Eval's candidate to the PM: the deadline counts from now.
    pub async fn candidate_delivered(&self, eval_id: &str) -> anyhow::Result<()> {
        self.command(async |cx| evals::delivered(cx, eval_id).await)
            .await
    }

    /// Decides a Trigger: the Eval that holds it, or a direct Eval of it alone. `eval_id` names the
    /// Eval the outcome was judged for, so a later Eval refuses it.
    pub async fn decide(
        &self,
        trigger_id: &str,
        outcome: Outcome,
        eval_id: Option<&str>,
    ) -> anyhow::Result<Decision> {
        self.command(async |cx| evals::decide(cx, trigger_id, outcome, eval_id).await)
            .await
    }

    /// Routes a Trigger to a ticket the message points at.
    pub async fn attach(
        &self,
        ticket_id: &str,
        trigger_id: &str,
        note: Option<&str>,
    ) -> anyhow::Result<Ticket> {
        self.command(async |cx| {
            let handed_back = cx
                .tx
                .disposition(trigger_id)
                .await?
                .and_then(|d| d.handbacks)
                .is_some_and(|items| items.iter().any(|item| item.ticket_id == ticket_id));
            if handed_back {
                return Err(refuse("handedBackHere", &[]));
            }
            let trigger = tickets::attachable(cx, ticket_id, trigger_id).await?;
            evals::withdraw(cx, &trigger).await?;
            tickets::attach(cx, ticket_id, &trigger, note).await?;
            evals::routed(cx, trigger_id, ticket_id).await?;
            tickets::wake(cx, ticket_id).await?;
            cx.require(ticket_id).await
        })
        .await
    }

    /// Asks a worker that waits for its status; answers whether it was asked.
    pub async fn request_status(&self, ticket_id: &str) -> anyhow::Result<bool> {
        self.command(async |cx| tickets::request_status(cx, ticket_id).await)
            .await
    }

    /// Gives an open ticket's worker a fact as a note Input, which grants nothing.
    pub async fn note(&self, ticket_id: &str, text: &str) -> anyhow::Result<Ticket> {
        self.command(async |cx| tickets::note(cx, ticket_id, text).await)
            .await
    }

    /// Queues a message for the conversation its purpose names.
    pub async fn reply(&self, request: ReplyRequest) -> anyhow::Result<Reply> {
        self.command(async |cx| replies::reply(cx, &request).await)
            .await
    }

    // ---- people ----

    /// Queues a stored Trigger for a fresh Eval; false while an Eval holds it.
    pub async fn reevaluate(&self, trigger_id: &str) -> anyhow::Result<bool> {
        self.command(async |cx| evals::reevaluate(cx, trigger_id).await)
            .await
    }

    /// Dismisses the problem an unavailable Decision shows.
    pub async fn dismiss(&self, decision_id: &str) -> anyhow::Result<Decision> {
        self.command(async |cx| evals::dismiss(cx, decision_id).await)
            .await
    }

    /// Drops every ticket's Subscription to a Thread; answers whether one was dropped.
    pub async fn unsubscribe(&self, thread_key: &str) -> anyhow::Result<bool> {
        self.command(async |cx| {
            tickets::drop_subscriptions(cx, thread_key, "person", None, None).await
        })
        .await
    }

    /// A person stops a ticket, which withdraws its Mandate.
    pub async fn stop_ticket(
        &self,
        ticket_id: &str,
        reason: Option<&str>,
    ) -> anyhow::Result<Ticket> {
        self.command(async |cx| tickets::stop(cx, ticket_id, reason).await)
            .await
    }

    /// A person retries a ticket whose worker reported failed or blocked, or whose delivery failed.
    pub async fn retry_ticket(&self, ticket_id: &str) -> anyhow::Result<Ticket> {
        self.command(async |cx| tickets::retry(cx, ticket_id).await)
            .await
    }

    // ---- the executor ----

    pub async fn assignment_started(
        &self,
        ticket_id: &str,
        assignment_id: &str,
    ) -> anyhow::Result<()> {
        self.command(async |cx| assignments::started(cx, ticket_id, assignment_id).await)
            .await
    }

    pub async fn inputs_delivered(
        &self,
        ticket_id: &str,
        assignment_id: &str,
        input_ids: &[String],
    ) -> anyhow::Result<()> {
        self.command(async |cx| {
            assignments::delivered(cx, ticket_id, assignment_id, input_ids).await
        })
        .await
    }

    pub async fn turn_started(&self, ticket_id: &str, assignment_id: &str) -> anyhow::Result<()> {
        self.command(async |cx| assignments::turn_started(cx, ticket_id, assignment_id).await)
            .await
    }

    pub async fn inputs_read(&self, ticket_id: &str, input_ids: &[String]) -> anyhow::Result<()> {
        self.command(async |cx| assignments::read(cx, ticket_id, input_ids).await)
            .await
    }

    pub async fn turn_ended(&self, ticket_id: &str, assignment_id: &str) -> anyhow::Result<()> {
        self.command(async |cx| assignments::turn_ended(cx, ticket_id, assignment_id).await)
            .await
    }

    pub async fn delivery_failed(
        &self,
        ticket_id: &str,
        assignment_id: &str,
        error: &str,
    ) -> anyhow::Result<()> {
        self.command(async |cx| {
            assignments::delivery_failed(cx, ticket_id, assignment_id, error).await
        })
        .await
    }

    pub async fn mark_unrelated(
        &self,
        ticket_id: &str,
        thread_key: &str,
        reason: &str,
    ) -> anyhow::Result<()> {
        self.command(async |cx| tickets::mark_unrelated(cx, ticket_id, thread_key, reason).await)
            .await
    }

    pub async fn follow_for_context(
        &self,
        ticket_id: &str,
        thread: ThreadRef,
    ) -> anyhow::Result<bool> {
        self.command(async |cx| tickets::follow_for_context(cx, ticket_id, &thread).await)
            .await
    }

    /// A worker hands back a message that reached its ticket but asks for more than it.
    pub async fn handback(
        &self,
        ticket_id: &str,
        trigger_id: &str,
        reason: &str,
    ) -> anyhow::Result<bool> {
        self.command(async |cx| evals::handback(cx, ticket_id, trigger_id, reason).await)
            .await
    }

    /// Records a report on an open ticket's current Assignment.
    pub async fn report(&self, ticket_id: &str, report: WorkerReport) -> anyhow::Result<Ticket> {
        self.command(async |cx| assignments::report(cx, ticket_id, &report).await)
            .await
    }

    /// An Assignment was running when the host stopped.
    pub async fn assignment_interrupted(
        &self,
        ticket_id: &str,
        assignment_id: &str,
    ) -> anyhow::Result<()> {
        self.command(async |cx| assignments::interrupted(cx, ticket_id, assignment_id).await)
            .await
    }

    // ---- settings ----

    /// The Policy changed: each open ticket in `effects` is told how, and Evals in progress start
    /// again under the new version. Install the new Policy with `set_policy` first.
    pub async fn policy_changed(
        &self,
        from: &str,
        to: &str,
        effects: HashMap<String, PolicyEffect>,
    ) -> anyhow::Result<()> {
        self.command(async |cx| {
            tickets::policy_changed(cx, from, to, &effects).await?;
            evals::policy_changed(cx, from, to).await
        })
        .await
    }

    // ---- lookups the intake needs ----

    /// The ticket a display name (`tNNN_short_name`) names, from the index the accept writes.
    pub async fn ticket_of_name(&self, name: &str) -> anyhow::Result<Option<String>> {
        self.inner
            .store
            .kv_get(&format!("ticket-name:{name}"))
            .await
    }

    /// The ticket whose worker runs in a thread, from the index the executor writes with the
    /// ticket's session record (`worker_thread_key`).
    pub async fn worker_ticket(&self, thread_id: &str) -> anyhow::Result<Option<String>> {
        self.inner.store.kv_get(&worker_thread_key(thread_id)).await
    }
}

/// The kv key of the reverse index from a worker's thread to its ticket. Intake reads it to place
/// a person's input in a worker thread on that ticket; the executor writes it with the session.
pub fn worker_thread_key(thread_id: &str) -> String {
    format!("worker-thread:{thread_id}")
}

/// What the PM asks of the engine (prototype `EngineForMainSession`).
pub trait EngineForPm: Send + Sync {
    fn candidate<'a>(
        &'a self,
        trigger_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<Option<Candidate>>>;
    fn candidate_delivered<'a>(&'a self, eval_id: &'a str) -> BoxFut<'a, anyhow::Result<()>>;
    fn decide<'a>(
        &'a self,
        trigger_id: &'a str,
        outcome: Outcome,
        eval_id: Option<String>,
    ) -> BoxFut<'a, anyhow::Result<Decision>>;
    fn attach<'a>(
        &'a self,
        ticket_id: &'a str,
        trigger_id: &'a str,
        note: Option<String>,
    ) -> BoxFut<'a, anyhow::Result<Ticket>>;
    fn request_status<'a>(&'a self, ticket_id: &'a str) -> BoxFut<'a, anyhow::Result<bool>>;
    fn note<'a>(&'a self, ticket_id: &'a str, text: &'a str) -> BoxFut<'a, anyhow::Result<Ticket>>;
    fn reply<'a>(&'a self, request: ReplyRequest) -> BoxFut<'a, anyhow::Result<Reply>>;
}

/// What people ask of the engine, through the PM or a status tool.
pub trait EngineForPeople: Send + Sync {
    fn stop_ticket<'a>(
        &'a self,
        ticket_id: &'a str,
        reason: Option<String>,
    ) -> BoxFut<'a, anyhow::Result<Ticket>>;
    fn retry_ticket<'a>(&'a self, ticket_id: &'a str) -> BoxFut<'a, anyhow::Result<Ticket>>;
    fn reevaluate<'a>(&'a self, trigger_id: &'a str) -> BoxFut<'a, anyhow::Result<bool>>;
    fn unsubscribe<'a>(&'a self, thread_key: &'a str) -> BoxFut<'a, anyhow::Result<bool>>;
    fn dismiss<'a>(&'a self, decision_id: &'a str) -> BoxFut<'a, anyhow::Result<Decision>>;
}

/// What the executor tells the engine, and what its workers ask of it.
pub trait EngineForExecutor: Send + Sync {
    fn assignment_started<'a>(
        &'a self,
        ticket_id: &'a str,
        assignment_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>>;
    fn inputs_delivered<'a>(
        &'a self,
        ticket_id: &'a str,
        assignment_id: &'a str,
        input_ids: Vec<String>,
    ) -> BoxFut<'a, anyhow::Result<()>>;
    /// A worker turn started on the assignment's thread.
    fn turn_started<'a>(
        &'a self,
        ticket_id: &'a str,
        assignment_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>>;
    fn inputs_read<'a>(
        &'a self,
        ticket_id: &'a str,
        input_ids: Vec<String>,
    ) -> BoxFut<'a, anyhow::Result<()>>;
    fn turn_ended<'a>(
        &'a self,
        ticket_id: &'a str,
        assignment_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>>;
    fn delivery_failed<'a>(
        &'a self,
        ticket_id: &'a str,
        assignment_id: &'a str,
        error: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>>;
    fn report<'a>(
        &'a self,
        ticket_id: &'a str,
        report: WorkerReport,
    ) -> BoxFut<'a, anyhow::Result<Ticket>>;
    fn mark_unrelated<'a>(
        &'a self,
        ticket_id: &'a str,
        thread_key: &'a str,
        reason: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>>;
    fn follow_for_context<'a>(
        &'a self,
        ticket_id: &'a str,
        thread: ThreadRef,
    ) -> BoxFut<'a, anyhow::Result<bool>>;
    fn handback<'a>(
        &'a self,
        ticket_id: &'a str,
        trigger_id: &'a str,
        reason: &'a str,
    ) -> BoxFut<'a, anyhow::Result<bool>>;
    fn assignment_interrupted<'a>(
        &'a self,
        ticket_id: &'a str,
        assignment_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>>;
    fn reply<'a>(&'a self, request: ReplyRequest) -> BoxFut<'a, anyhow::Result<Reply>>;
}

impl EngineForPm for Engine {
    fn candidate<'a>(
        &'a self,
        trigger_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<Option<Candidate>>> {
        Box::pin(Engine::candidate(self, trigger_id))
    }

    fn candidate_delivered<'a>(&'a self, eval_id: &'a str) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(Engine::candidate_delivered(self, eval_id))
    }

    fn decide<'a>(
        &'a self,
        trigger_id: &'a str,
        outcome: Outcome,
        eval_id: Option<String>,
    ) -> BoxFut<'a, anyhow::Result<Decision>> {
        Box::pin(async move { Engine::decide(self, trigger_id, outcome, eval_id.as_deref()).await })
    }

    fn attach<'a>(
        &'a self,
        ticket_id: &'a str,
        trigger_id: &'a str,
        note: Option<String>,
    ) -> BoxFut<'a, anyhow::Result<Ticket>> {
        Box::pin(async move { Engine::attach(self, ticket_id, trigger_id, note.as_deref()).await })
    }

    fn request_status<'a>(&'a self, ticket_id: &'a str) -> BoxFut<'a, anyhow::Result<bool>> {
        Box::pin(Engine::request_status(self, ticket_id))
    }

    fn note<'a>(&'a self, ticket_id: &'a str, text: &'a str) -> BoxFut<'a, anyhow::Result<Ticket>> {
        Box::pin(Engine::note(self, ticket_id, text))
    }

    fn reply<'a>(&'a self, request: ReplyRequest) -> BoxFut<'a, anyhow::Result<Reply>> {
        Box::pin(Engine::reply(self, request))
    }
}

impl EngineForPeople for Engine {
    fn stop_ticket<'a>(
        &'a self,
        ticket_id: &'a str,
        reason: Option<String>,
    ) -> BoxFut<'a, anyhow::Result<Ticket>> {
        Box::pin(async move { Engine::stop_ticket(self, ticket_id, reason.as_deref()).await })
    }

    fn retry_ticket<'a>(&'a self, ticket_id: &'a str) -> BoxFut<'a, anyhow::Result<Ticket>> {
        Box::pin(Engine::retry_ticket(self, ticket_id))
    }

    fn reevaluate<'a>(&'a self, trigger_id: &'a str) -> BoxFut<'a, anyhow::Result<bool>> {
        Box::pin(Engine::reevaluate(self, trigger_id))
    }

    fn unsubscribe<'a>(&'a self, thread_key: &'a str) -> BoxFut<'a, anyhow::Result<bool>> {
        Box::pin(Engine::unsubscribe(self, thread_key))
    }

    fn dismiss<'a>(&'a self, decision_id: &'a str) -> BoxFut<'a, anyhow::Result<Decision>> {
        Box::pin(Engine::dismiss(self, decision_id))
    }
}

impl EngineForExecutor for Engine {
    fn assignment_started<'a>(
        &'a self,
        ticket_id: &'a str,
        assignment_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(Engine::assignment_started(self, ticket_id, assignment_id))
    }

    fn inputs_delivered<'a>(
        &'a self,
        ticket_id: &'a str,
        assignment_id: &'a str,
        input_ids: Vec<String>,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(async move {
            Engine::inputs_delivered(self, ticket_id, assignment_id, &input_ids).await
        })
    }

    fn turn_started<'a>(
        &'a self,
        ticket_id: &'a str,
        assignment_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(Engine::turn_started(self, ticket_id, assignment_id))
    }

    fn inputs_read<'a>(
        &'a self,
        ticket_id: &'a str,
        input_ids: Vec<String>,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(async move { Engine::inputs_read(self, ticket_id, &input_ids).await })
    }

    fn turn_ended<'a>(
        &'a self,
        ticket_id: &'a str,
        assignment_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(Engine::turn_ended(self, ticket_id, assignment_id))
    }

    fn delivery_failed<'a>(
        &'a self,
        ticket_id: &'a str,
        assignment_id: &'a str,
        error: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(Engine::delivery_failed(
            self,
            ticket_id,
            assignment_id,
            error,
        ))
    }

    fn report<'a>(
        &'a self,
        ticket_id: &'a str,
        report: WorkerReport,
    ) -> BoxFut<'a, anyhow::Result<Ticket>> {
        Box::pin(Engine::report(self, ticket_id, report))
    }

    fn mark_unrelated<'a>(
        &'a self,
        ticket_id: &'a str,
        thread_key: &'a str,
        reason: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(Engine::mark_unrelated(self, ticket_id, thread_key, reason))
    }

    fn follow_for_context<'a>(
        &'a self,
        ticket_id: &'a str,
        thread: ThreadRef,
    ) -> BoxFut<'a, anyhow::Result<bool>> {
        Box::pin(Engine::follow_for_context(self, ticket_id, thread))
    }

    fn handback<'a>(
        &'a self,
        ticket_id: &'a str,
        trigger_id: &'a str,
        reason: &'a str,
    ) -> BoxFut<'a, anyhow::Result<bool>> {
        Box::pin(Engine::handback(self, ticket_id, trigger_id, reason))
    }

    fn assignment_interrupted<'a>(
        &'a self,
        ticket_id: &'a str,
        assignment_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(Engine::assignment_interrupted(
            self,
            ticket_id,
            assignment_id,
        ))
    }

    fn reply<'a>(&'a self, request: ReplyRequest) -> BoxFut<'a, anyhow::Result<Reply>> {
        Box::pin(Engine::reply(self, request))
    }
}

// ---- in-transaction entry points for the executor (integration) ----

impl Engine {
    /// Runs `work` as a command inside the caller's transaction, for the executor, which records
    /// its own facts (the inflight marker, the session record) in the same transaction as the
    /// engine's. The command's events go out once that transaction commits.
    pub(crate) async fn command_in<T>(
        &self,
        tx: &mut Tx,
        work: impl AsyncFnOnce(&mut Cx<'_>) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let now = (self.inner.now)();
        let policy = self.policy();
        let inner = Arc::clone(&self.inner);
        let (value, events) = {
            let mut cx = Cx {
                e: inner.as_ref(),
                tx: &mut *tx,
                now,
                policy,
                events: Vec::new(),
            };
            let value = work(&mut cx).await?;
            (value, std::mem::take(&mut cx.events))
        };
        let sender = self.inner.events.clone();
        tx.after_commit(move || {
            for event in events {
                let _ = sender.send(event);
            }
        });
        Ok(value)
    }

    pub async fn assignment_started_in(
        &self,
        tx: &mut Tx,
        ticket_id: &str,
        assignment_id: &str,
    ) -> anyhow::Result<()> {
        self.command_in(tx, async |cx| {
            assignments::started(cx, ticket_id, assignment_id).await
        })
        .await
    }

    pub async fn inputs_delivered_in(
        &self,
        tx: &mut Tx,
        ticket_id: &str,
        assignment_id: &str,
        input_ids: &[String],
    ) -> anyhow::Result<()> {
        self.command_in(tx, async |cx| {
            assignments::delivered(cx, ticket_id, assignment_id, input_ids).await
        })
        .await
    }

    pub async fn turn_started_in(
        &self,
        tx: &mut Tx,
        ticket_id: &str,
        assignment_id: &str,
    ) -> anyhow::Result<()> {
        self.command_in(tx, async |cx| {
            assignments::turn_started(cx, ticket_id, assignment_id).await
        })
        .await
    }

    pub async fn inputs_read_in(
        &self,
        tx: &mut Tx,
        ticket_id: &str,
        input_ids: &[String],
    ) -> anyhow::Result<()> {
        self.command_in(tx, async |cx| {
            assignments::read(cx, ticket_id, input_ids).await
        })
        .await
    }

    pub async fn turn_ended_in(
        &self,
        tx: &mut Tx,
        ticket_id: &str,
        assignment_id: &str,
    ) -> anyhow::Result<()> {
        self.command_in(tx, async |cx| {
            assignments::turn_ended(cx, ticket_id, assignment_id).await
        })
        .await
    }

    pub async fn delivery_failed_in(
        &self,
        tx: &mut Tx,
        ticket_id: &str,
        assignment_id: &str,
        error: &str,
    ) -> anyhow::Result<()> {
        self.command_in(tx, async |cx| {
            assignments::delivery_failed(cx, ticket_id, assignment_id, error).await
        })
        .await
    }

    pub async fn report_in(
        &self,
        tx: &mut Tx,
        ticket_id: &str,
        report: WorkerReport,
    ) -> anyhow::Result<Ticket> {
        self.command_in(tx, async |cx| {
            assignments::report(cx, ticket_id, &report).await
        })
        .await
    }

    pub async fn mark_unrelated_in(
        &self,
        tx: &mut Tx,
        ticket_id: &str,
        thread_key: &str,
        reason: &str,
    ) -> anyhow::Result<()> {
        self.command_in(tx, async |cx| {
            tickets::mark_unrelated(cx, ticket_id, thread_key, reason).await
        })
        .await
    }

    pub async fn follow_for_context_in(
        &self,
        tx: &mut Tx,
        ticket_id: &str,
        thread: ThreadRef,
    ) -> anyhow::Result<bool> {
        self.command_in(tx, async |cx| {
            tickets::follow_for_context(cx, ticket_id, &thread).await
        })
        .await
    }

    pub async fn handback_in(
        &self,
        tx: &mut Tx,
        ticket_id: &str,
        trigger_id: &str,
        reason: &str,
    ) -> anyhow::Result<bool> {
        self.command_in(tx, async |cx| {
            evals::handback(cx, ticket_id, trigger_id, reason).await
        })
        .await
    }

    pub async fn assignment_interrupted_in(
        &self,
        tx: &mut Tx,
        ticket_id: &str,
        assignment_id: &str,
    ) -> anyhow::Result<()> {
        self.command_in(tx, async |cx| {
            assignments::interrupted(cx, ticket_id, assignment_id).await
        })
        .await
    }

    pub async fn reply_in(&self, tx: &mut Tx, request: ReplyRequest) -> anyhow::Result<Reply> {
        self.command_in(tx, async |cx| replies::reply(cx, &request).await)
            .await
    }
}

impl Engine {
    /// A fact the executor gives a ticket's worker, such as a background command that ended, as an
    /// Input; it asks for nothing and grants nothing.
    pub async fn note_in(
        &self,
        tx: &mut Tx,
        ticket_id: &str,
        text: &str,
    ) -> anyhow::Result<Ticket> {
        self.command_in(tx, async |cx| tickets::note(cx, ticket_id, text).await)
            .await
    }
}
