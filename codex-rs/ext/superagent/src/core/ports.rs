//! What the engine asks of its surroundings and tells them (prototype `src/core/ports.ts`). The
//! durable ports (launch, delivery, assessment, reply) are effect rows the engine writes itself; the
//! in-memory ones are the `EngineHooks` callbacks, run after a command commits.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;

use super::model::Assignment;
use super::model::Mandate;
use super::model::MandateBasis;
use super::model::ReportState;
use super::model::Strength;
use super::model::Thread;
use super::model::Ticket;
use super::model::TicketEndBy;
use super::model::TicketOutcome;
use super::model::Trigger;
use super::reasons::Reason;
use crate::store::ReplyStatus;

/// Gate admission: inadmissible with a reason code, or admissible with a strength and the basis a Mandate records.
#[derive(Clone, Debug, PartialEq)]
pub enum GateResult {
    Admissible {
        strength: Strength,
        basis: MandateBasis,
    },
    Inadmissible {
        reason: String,
    },
}

/// Whether a Source receives a Trigger; a Trigger it does not listen to is stored as `filtered`.
#[derive(Clone, Debug, PartialEq)]
pub enum ScopeResult {
    Listened { observed: bool },
    Filtered { reason: String },
}

pub trait SourceScope: Send + Sync {
    fn scope(&self, trigger: &Trigger) -> ScopeResult;
}

/// The single source of A1 listens to everything.
pub struct ListenAll;

impl SourceScope for ListenAll {
    fn scope(&self, _trigger: &Trigger) -> ScopeResult {
        ScopeResult::Listened { observed: false }
    }
}

/// Versioned rules: who may originate work, and whether a Mandate still holds.
pub trait Policy: Send + Sync {
    fn version(&self) -> String;
    fn paused(&self) -> bool;
    /// How long an assessor may take before an Eval is recorded unavailable.
    fn eval_timeout_ms(&self) -> i64;
    fn gate(&self, trigger: &Trigger, thread: &Thread, tickets: &[Ticket]) -> GateResult;
    /// Re-check a Mandate's basis under this Policy, such as before an ended ticket resumes.
    fn revalidate(&self, mandate: &Mandate) -> bool;
    /// Whether an inferred candidate may be accepted on the rules its assessment matched.
    fn admits_inferred(&self, rules: &[String]) -> bool;
}

/// What the assessor (the PM) receives: bounded facts, never authority.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub eval_id: String,
    pub thread: Thread,
    pub triggers: Vec<Trigger>,
    pub strength: Strength,
    pub policy_version: String,
    /// Every ticket on the thread, including ended ones, for duplicate judgement.
    pub tickets: Vec<Ticket>,
}

/// An outcome's reason: a code it carries, else the words its assessor or a person wrote.
#[derive(Clone, Debug, PartialEq)]
pub enum OutcomeReason {
    Said(String),
    Code(Reason),
}

impl OutcomeReason {
    pub fn reasons(&self) -> Vec<Reason> {
        match self {
            Self::Said(text) => vec![Reason::said(text)],
            Self::Code(reason) => vec![reason.clone()],
        }
    }

    /// The words a person wrote, when the reason is words.
    pub fn words(&self) -> Option<&str> {
        match self {
            Self::Said(text) => Some(text),
            Self::Code(_) => None,
        }
    }
}

impl From<&str> for OutcomeReason {
    fn from(text: &str) -> Self {
        Self::Said(text.to_owned())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AcceptOutcome {
    pub title: String,
    pub scope: String,
    pub reason: OutcomeReason,
    pub acknowledgement: Option<String>,
    /// The rules an assessment matched, for an inferred candidate.
    pub rules: Option<Vec<String>>,
    pub audit: Option<Map<String, Value>>,
    /// The ticket's requested short name, and a routing note for its worker.
    pub name: Option<String>,
    pub note: Option<String>,
}

impl AcceptOutcome {
    pub fn new(title: &str, scope: &str, reason: &str) -> Self {
        Self {
            title: title.to_owned(),
            scope: scope.to_owned(),
            reason: OutcomeReason::from(reason),
            acknowledgement: None,
            rules: None,
            audit: None,
            name: None,
            note: None,
        }
    }

    pub fn acknowledge(mut self, text: &str) -> Self {
        self.acknowledgement = Some(text.to_owned());
        self
    }

    pub fn rules(mut self, rules: &[&str]) -> Self {
        self.rules = Some(rules.iter().map(|r| (*r).to_owned()).collect());
        self
    }
}

/// What the PM decides about a candidate, or a direct decision about an explicit request.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Ignore {
        reason: OutcomeReason,
        audit: Option<Map<String, Value>>,
    },
    Unavailable {
        reason: OutcomeReason,
        audit: Option<Map<String, Value>>,
    },
    Accept(AcceptOutcome),
    Stop {
        ticket_id: String,
        reason: OutcomeReason,
        audit: Option<Map<String, Value>>,
    },
}

impl Outcome {
    pub fn ignore(reason: &str) -> Self {
        Self::Ignore {
            reason: reason.into(),
            audit: None,
        }
    }

    pub fn stop(ticket_id: &str, reason: &str) -> Self {
        Self::Stop {
            ticket_id: ticket_id.to_owned(),
            reason: reason.into(),
            audit: None,
        }
    }

    pub fn audit(&self) -> Option<&Map<String, Value>> {
        match self {
            Self::Ignore { audit, .. }
            | Self::Unavailable { audit, .. }
            | Self::Stop { audit, .. } => audit.as_ref(),
            Self::Accept(accept) => accept.audit.as_ref(),
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Ignore { .. } => "ignore",
            Self::Unavailable { .. } => "unavailable",
            Self::Accept(_) => "accept",
            Self::Stop { .. } => "stop",
        }
    }
}

/// What an Assignment reports: a worker's report, or the executor's own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerReport {
    pub state: ReportState,
    pub summary: String,
    /// `None` keeps the current question, `Some(None)` clears it, `Some(Some(q))` asks it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<Option<String>>,
    /// The person's message whose request raised the question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asked_by: Option<String>,
    /// The person's question this report answers, which the worker thereby takes up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answers: Option<String>,
}

impl WorkerReport {
    pub fn new(state: ReportState, summary: &str) -> Self {
        Self {
            state,
            summary: summary.to_owned(),
            question: None,
            asked_by: None,
            answers: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyPurpose {
    Acknowledgement,
    Answer,
    Result,
    Attention,
}

/// A message for a conversation, by purpose (prototype `ReplyRequest`).
#[derive(Clone, Debug, PartialEq)]
pub struct ReplyRequest {
    pub purpose: ReplyPurpose,
    pub ticket_id: Option<String>,
    pub trigger_id: Option<String>,
    /// For attention: the ticket's current question, which the reply must match.
    pub question: Option<String>,
    pub text: String,
}

/// A queued message: its id, whether it went out, and whether an earlier request had queued it already.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub id: String,
    pub status: ReplyStatus,
    pub reused: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiveResult {
    Stored,
    Duplicate,
    Filtered,
}

/// The in-memory half of the executor and assessor ports: what runs after a command commits.
pub trait EngineHooks: Send + Sync {
    /// An Assignment's work ends: interrupt its turn, clean its background commands, stop its children.
    fn assignment_stopped(&self, _ticket: &Ticket, _assignment: &Assignment, _reason: &str) {}
    /// An Eval ended, decided or not: drop what the assessor kept for it.
    fn eval_released(&self, _eval_id: &str) {}
}

pub struct NoHooks;

impl EngineHooks for NoHooks {}

/// What the engine reports after a change.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum DomainEvent {
    #[serde(rename = "trigger.received")]
    TriggerReceived {
        trigger_id: String,
        threads: Vec<String>,
    },
    #[serde(rename = "eval.started")]
    EvalStarted {
        eval_id: String,
        thread_key: String,
        trigger_ids: Vec<String>,
        strength: Strength,
    },
    #[serde(rename = "decision.recorded")]
    DecisionRecorded {
        decision_id: String,
        kind: String,
        thread_key: String,
    },
    #[serde(rename = "ticket.created")]
    TicketCreated {
        ticket_id: String,
        decision_id: String,
        strength: Strength,
    },
    #[serde(rename = "ticket.resumed")]
    TicketResumed {
        ticket_id: String,
        trigger_id: String,
    },
    #[serde(rename = "ticket.ended")]
    TicketEnded {
        ticket_id: String,
        by: TicketEndBy,
        outcome: TicketOutcome,
        reason: Option<String>,
    },
    #[serde(rename = "assignment.started")]
    AssignmentStarted {
        ticket_id: String,
        assignment_id: String,
    },
    #[serde(rename = "assignment.ended")]
    AssignmentEnded {
        ticket_id: String,
        assignment_id: String,
        reason: String,
    },
    #[serde(rename = "thread.subscribed")]
    ThreadSubscribed {
        thread_key: String,
        ticket_id: String,
    },
    #[serde(rename = "thread.unsubscribed")]
    ThreadUnsubscribed {
        thread_key: String,
        ticket_id: String,
        by: String,
        reason: Option<String>,
    },
    #[serde(rename = "trigger.requeued")]
    TriggerRequeued { trigger_id: String },
    #[serde(rename = "trigger.followup")]
    TriggerFollowup { trigger_id: String },
    #[serde(rename = "policy.changed")]
    PolicyChanged { from: String, to: String },
}

/// A domain event with when it happened.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Event {
    pub at: String,
    #[serde(flatten)]
    pub event: DomainEvent,
}
