//! The executor port (design §6.7): the prototype's `Executor` contract from `src/core/ports.ts`,
//! unchanged, and the engine callbacks its workers report through (`EngineForExecutor`, the role
//! table of `docs/007-architecture.md`). `CodextThreadExecutor` is the first implementation.
//!
//! Every callback takes the caller's transaction, as the prototype's engine ran inside the
//! executor's `atomic`: the executor records its own facts (the `inflight` row, the session record)
//! and the engine's in one commit.

pub mod accounts;
pub mod background;
pub mod codext_thread;
pub mod slots;

use serde::Deserialize;
use serde::Serialize;

pub use codext_thread::CodextThreadExecutor;
pub use codext_thread::ExecutorConfig;
pub use codext_thread::ExecutorLifecycle;
pub use codext_thread::install_executor;
pub use slots::SlotBy;
pub use slots::WorkerSlots;

use crate::BoxFut;
use crate::core::Assignment;
use crate::core::ReportState;
use crate::core::Ticket;
use crate::store::Tx;

/// What a worker reports: the five-line format of the design (§6.3) plus the prototype's question
/// and answers.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkerReport {
    pub state: ReportState,
    /// At most 600 characters.
    pub summary: String,
    #[serde(default)]
    pub changes: Vec<String>,
    #[serde(default)]
    pub tests_run: Vec<String>,
    #[serde(default)]
    pub risks: Vec<String>,
    /// Paths under the ticket's evidence directory; `ready` needs at least one.
    #[serde(default)]
    pub evidence: Vec<String>,
    /// A question for a person, which makes the ticket wait for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// The person's message the question answers or replies to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// The people's questions this report answers.
    #[serde(default)]
    pub answers: Vec<String>,
}

/// Executes assignments; concurrency and workspace management are its own concern. The engine
/// calls it inside its commands, so each method only queues work, which reports back through
/// `EngineForExecutor`.
pub trait Executor: Send + Sync {
    /// Queues a pending assignment for launch; reports back through `assignment_started` or a
    /// `failed` report.
    fn start<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket: &'a Ticket,
        assignment: &'a Assignment,
    ) -> BoxFut<'a, anyhow::Result<()>>;
    /// Queues delivery of the ticket's waiting inputs to its running assignment.
    fn deliver<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket: &'a Ticket,
        assignment: &'a Assignment,
    ) -> BoxFut<'a, anyhow::Result<()>>;
    /// Ends an assignment's work once the commit goes through: the turn, its background commands
    /// and its subagents; called after the ticket or assignment has already ended in the domain.
    fn stop<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket: &'a Ticket,
        assignment: &'a Assignment,
        reason: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>>;
}

/// The facts an executor and its workers report to the engine (prototype `EngineForExecutor`).
pub trait EngineForExecutor: Send + Sync {
    /// An assignment runs; its waiting inputs go out.
    fn assignment_started<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        assignment_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>>;
    /// Inputs reached the worker's thread, where each waits for a step to take it.
    fn inputs_delivered<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        assignment_id: &'a str,
        input_ids: &'a [String],
    ) -> BoxFut<'a, anyhow::Result<()>>;
    /// A worker turn started on the assignment's thread.
    fn turn_started<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        assignment_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>>;
    /// The worker's thread took the messages carrying these inputs into a step.
    fn inputs_read<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        input_ids: &'a [String],
    ) -> BoxFut<'a, anyhow::Result<()>>;
    /// A turn of the worker's thread ended with nothing left to run for it.
    fn turn_ended<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        assignment_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>>;
    /// Delivering inputs failed; the ticket shows it until a delivery succeeds.
    fn delivery_failed<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        assignment_id: &'a str,
        error: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>>;
    /// Records a report and its question for a person.
    fn report<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        report: WorkerReport,
    ) -> BoxFut<'a, anyhow::Result<()>>;
    /// The worker acknowledged a person's request, taking it up as within the ticket.
    fn acknowledge<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        trigger_id: &'a str,
        text: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>>;
    /// Drops the ticket's subscription to an attached thread it found unrelated.
    fn mark_unrelated<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        thread_key: &'a str,
        reason: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>>;
    /// Subscribes the ticket to a related thread for context.
    fn follow_for_context<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        thread_key: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>>;
    /// Adds a note to the ticket's inputs; the engine delivers it to the current assignment.
    fn note<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        text: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>>;
    /// Hands a trigger back for a fresh assessment; answers whether it was queued.
    fn handback<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        trigger_id: &'a str,
        reason: &'a str,
    ) -> BoxFut<'a, anyhow::Result<bool>>;
    /// An assignment was running when the host stopped.
    fn assignment_interrupted<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        assignment_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>>;
    /// A background command of the worker ended: the engine records it as an input of the ticket,
    /// which wakes the worker when it is idle (design §1.4).
    fn background_ended<'a>(
        &'a self,
        _tx: &'a mut Tx,
        _ticket_id: &'a str,
        _text: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}
