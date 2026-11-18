//! Glue between the three parts built in parallel: the engine (T-12), the PM side (T-13) and the
//! executor (T-14). Each adapter translates one side's contract onto the engine; none holds rules
//! of its own.

use crate::BoxFut;
use crate::core;
use crate::core::Engine;
use crate::core::ThreadRef;
use crate::executor;
use crate::executor::CodextThreadExecutor;
use crate::executor::Executor;
use crate::pm;
use crate::store::Store;
use crate::store::Tx;

/// The engine as the executor calls it: every fact lands in the executor's own transaction, next
/// to the inflight marker and the session record it keeps there.
pub struct ExecutorEngine(pub Engine);

fn thread_ref(key: &str) -> ThreadRef {
    match key.split_once(':') {
        Some((source, id)) => ThreadRef {
            source: source.to_owned(),
            id: id.to_owned(),
        },
        None => ThreadRef {
            source: "codext".to_owned(),
            id: key.to_owned(),
        },
    }
}

/// The worker's five-line report as the engine records it: the state and summary, the question
/// with the message that raised it, and the person's question it answers.
fn worker_report(report: executor::WorkerReport) -> core::WorkerReport {
    if report.answers.len() > 1 {
        tracing::warn!(
            "superagent: a report answers {} questions; the engine records the first",
            report.answers.len()
        );
    }
    core::WorkerReport {
        state: report.state,
        summary: report.summary,
        question: report.question.map(Some),
        asked_by: report.reply_to,
        answers: report.answers.into_iter().next(),
    }
}

impl executor::EngineForExecutor for ExecutorEngine {
    fn assignment_started<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        assignment_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(self.0.assignment_started_in(tx, ticket_id, assignment_id))
    }

    fn inputs_delivered<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        assignment_id: &'a str,
        input_ids: &'a [String],
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(
            self.0
                .inputs_delivered_in(tx, ticket_id, assignment_id, input_ids),
        )
    }

    fn turn_started<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        assignment_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(self.0.turn_started_in(tx, ticket_id, assignment_id))
    }

    fn inputs_read<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        input_ids: &'a [String],
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(self.0.inputs_read_in(tx, ticket_id, input_ids))
    }

    fn turn_ended<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        assignment_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(self.0.turn_ended_in(tx, ticket_id, assignment_id))
    }

    fn delivery_failed<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        assignment_id: &'a str,
        error: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(
            self.0
                .delivery_failed_in(tx, ticket_id, assignment_id, error),
        )
    }

    fn report<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        report: executor::WorkerReport,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(async move {
            self.0
                .report_in(tx, ticket_id, worker_report(report))
                .await
                .map(|_| ())
        })
    }

    fn acknowledge<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        trigger_id: &'a str,
        text: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let request = core::ReplyRequest {
                purpose: core::ReplyPurpose::Acknowledgement,
                ticket_id: Some(ticket_id.to_owned()),
                trigger_id: Some(trigger_id.to_owned()),
                question: None,
                text: text.to_owned(),
            };
            self.0.reply_in(tx, request).await.map(|_| ())
        })
    }

    fn mark_unrelated<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        thread_key: &'a str,
        reason: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(self.0.mark_unrelated_in(tx, ticket_id, thread_key, reason))
    }

    fn follow_for_context<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        thread_key: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(async move {
            self.0
                .follow_for_context_in(tx, ticket_id, thread_ref(thread_key))
                .await
                .map(|_| ())
        })
    }

    fn note<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        text: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(async move { self.0.note_in(tx, ticket_id, text).await.map(|_| ()) })
    }

    fn handback<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        trigger_id: &'a str,
        reason: &'a str,
    ) -> BoxFut<'a, anyhow::Result<bool>> {
        Box::pin(self.0.handback_in(tx, ticket_id, trigger_id, reason))
    }

    fn assignment_interrupted<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        assignment_id: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(
            self.0
                .assignment_interrupted_in(tx, ticket_id, assignment_id),
        )
    }

    fn background_ended<'a>(
        &'a self,
        tx: &'a mut Tx,
        ticket_id: &'a str,
        text: &'a str,
    ) -> BoxFut<'a, anyhow::Result<()>> {
        Box::pin(async move { self.0.note_in(tx, ticket_id, text).await.map(|_| ()) })
    }
}

/// The engine as the PM's tools call it.
pub struct PmEngine(pub Engine);

fn engine_error(err: anyhow::Error) -> pm::EngineError {
    match core::refusal(&err) {
        Some(refusal) => pm::EngineError::Refused(refusal.clone()),
        None => pm::EngineError::Other(format!("{err:#}")),
    }
}

fn candidate(candidate: core::Candidate) -> pm::Candidate {
    pm::Candidate {
        eval_id: candidate.eval_id,
        thread: candidate.thread,
        triggers: candidate.triggers,
        strength: candidate.strength,
        policy_version: candidate.policy_version,
        tickets: candidate.tickets,
        // A1 admits explicit requests only; inferred candidates are recorded, not accepted.
        mode: core::AcceptMode::Observe,
    }
}

fn outcome_reason(reason: core::Reason) -> core::OutcomeReason {
    if reason.code == "said" {
        let text = reason
            .values
            .as_ref()
            .and_then(|values| values.get("text").cloned())
            .unwrap_or_default();
        core::OutcomeReason::Said(text)
    } else {
        core::OutcomeReason::Code(reason)
    }
}

fn outcome(outcome: pm::RouteOutcome) -> core::Outcome {
    match outcome {
        pm::RouteOutcome::Accept {
            title,
            scope,
            reason,
            acknowledgement,
            name,
            note,
        } => core::Outcome::Accept(core::AcceptOutcome {
            title,
            scope,
            reason: outcome_reason(reason),
            acknowledgement,
            rules: None,
            audit: None,
            name,
            note,
        }),
        pm::RouteOutcome::Stop { ticket_id, reason } => core::Outcome::Stop {
            ticket_id,
            reason: outcome_reason(reason),
            audit: None,
        },
        pm::RouteOutcome::Ignore { reason } => core::Outcome::Ignore {
            reason: outcome_reason(reason),
            audit: None,
        },
    }
}

impl pm::EngineForPm for PmEngine {
    fn candidate(
        &self,
        trigger_id: String,
    ) -> BoxFut<'_, Result<Option<pm::Candidate>, pm::EngineError>> {
        Box::pin(async move {
            self.0
                .candidate(&trigger_id)
                .await
                .map(|found| found.map(candidate))
                .map_err(engine_error)
        })
    }

    fn candidate_delivered(&self, eval_id: String) -> BoxFut<'_, Result<(), pm::EngineError>> {
        Box::pin(async move {
            self.0
                .candidate_delivered(&eval_id)
                .await
                .map_err(engine_error)
        })
    }

    fn decide(
        &self,
        trigger_id: String,
        route: pm::RouteOutcome,
    ) -> BoxFut<'_, Result<core::Decision, pm::EngineError>> {
        Box::pin(async move {
            self.0
                .decide(&trigger_id, outcome(route), None)
                .await
                .map_err(engine_error)
        })
    }

    fn attach(
        &self,
        ticket_id: String,
        trigger_id: String,
        note: Option<String>,
    ) -> BoxFut<'_, Result<core::Ticket, pm::EngineError>> {
        Box::pin(async move {
            self.0
                .attach(&ticket_id, &trigger_id, note.as_deref())
                .await
                .map_err(engine_error)
        })
    }

    fn request_status(&self, ticket_id: String) -> BoxFut<'_, Result<bool, pm::EngineError>> {
        Box::pin(async move {
            self.0
                .request_status(&ticket_id)
                .await
                .map_err(engine_error)
        })
    }

    fn note(&self, ticket_id: String, text: String) -> BoxFut<'_, Result<(), pm::EngineError>> {
        Box::pin(async move {
            self.0
                .note(&ticket_id, &text)
                .await
                .map(|_| ())
                .map_err(engine_error)
        })
    }

    fn retry_ticket(&self, ticket_id: String) -> BoxFut<'_, Result<core::Ticket, pm::EngineError>> {
        Box::pin(async move { self.0.retry_ticket(&ticket_id).await.map_err(engine_error) })
    }

    fn reevaluate(&self, trigger_id: String) -> BoxFut<'_, Result<bool, pm::EngineError>> {
        Box::pin(async move { self.0.reevaluate(&trigger_id).await.map_err(engine_error) })
    }
}

/// What the engine asks the executor to do in memory once a command committed: stop the work of
/// an assignment it ended.
pub struct ExecutorHooks {
    pub executor: CodextThreadExecutor,
    pub store: Store,
}

impl core::EngineHooks for ExecutorHooks {
    fn assignment_stopped(
        &self,
        ticket: &core::Ticket,
        assignment: &core::Assignment,
        reason: &str,
    ) {
        let executor = self.executor.clone();
        let store = self.store.clone();
        let ticket = ticket.clone();
        let assignment = assignment.clone();
        let reason = reason.to_owned();
        tokio::spawn(async move {
            let stopped = store
                .atomic(async |tx| executor.stop(tx, &ticket, &assignment, &reason).await)
                .await;
            if let Err(err) = stopped {
                tracing::warn!(
                    "superagent: stopping the work of {} failed: {err:#}",
                    ticket.id
                );
            }
        });
    }
}

