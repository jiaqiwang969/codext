//! Domain model: the concepts and their invariants, independent of any source, policy or executor
//! (prototype `src/core/model.ts`). The JSON shape is the prototype's, so the generated columns of
//! the store read the same paths.

use std::collections::BTreeMap;

use chrono::DateTime;
use chrono::SecondsFormat;
use chrono::Utc;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;

use super::reasons::Reason;

/// Actor identity in the source's own vocabulary.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorRef {
    pub source: String,
    pub id: String,
}

/// Thread identity; the rule that derives `id` from a native event belongs to the source.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadRef {
    pub source: String,
    pub id: String,
}

/// Write-only address on a thread; the address format belongs to the source.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelRef {
    pub source: String,
    pub thread: String,
    #[serde(default)]
    pub address: BTreeMap<String, String>,
}

/// Stable key of a thread across the whole domain.
pub fn thread_key(reference: &ThreadRef) -> String {
    format!("{}:{}", reference.source, reference.id)
}

/// Normalizes a timestamp to the form `Date#toISOString` gives, so stored timestamps compare as text;
/// a value that does not parse is returned unchanged.
pub fn iso_time(value: &str) -> String {
    DateTime::parse_from_rfc3339(value)
        .map(|t| {
            t.with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Millis, true)
        })
        .unwrap_or_else(|_| value.to_owned())
}

/// The current time in the one form the store compares.
pub fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Addressed {
    Mention,
    Direct,
}

/// One immutable event produced by a source. `text` and `meta` are untrusted external data.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Trigger {
    pub id: String,
    pub source: String,
    /// Every thread the trigger belongs to; the first is the primary thread that hosts its evaluation.
    pub threads: Vec<ThreadRef>,
    pub actor: ActorRef,
    /// Source-declared: written by a person rather than a bot, CI or this agent.
    pub human: bool,
    /// Source-declared: the sender addressed the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addressed: Option<Addressed>,
    pub at: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<ChannelRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Source-native details for adapters; the core never interprets them.
    #[serde(default)]
    pub meta: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handback {
    pub ticket_id: String,
    pub reason: String,
    pub at: String,
}

/// Where a trigger went after intake; it grants nothing, but decides who decides a person's follow-up.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TriggerDisposition {
    /// Rejected by the source scope filter before entering the domain, for the Source's reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filtered: Option<Reason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
    /// The tickets it reached, by subscription fan-out or attached by the PM.
    #[serde(default)]
    pub fanout: Vec<String>,
    /// The tickets of `fanout` it reached only on a Thread their worker follows for context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Vec<String>>,
    /// A person, or the PM for a message no ticket decides, sent it to Eval again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requeued: Option<bool>,
    /// The tickets whose worker handed it back as beyond the ticket, each once, with why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handbacks: Option<Vec<Handback>>,
    /// The tickets whose worker acknowledged it or answered it, taking it up as within the ticket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked: Option<Vec<String>>,
    pub at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strength {
    Explicit,
    Inferred,
}

/// Whether proactive candidates are assessed and accepted (`auto`), assessed but only recorded
/// (`observe`), or not admitted (`off`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AcceptMode {
    Off,
    Observe,
    Auto,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<AcceptMode>,
}

/// How the gate admitted the Trigger a Mandate came from: `via` is `addressed`, `followup`,
/// `admission`, `listed` or `bot`; the other fields are those the prototype records for that `via`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MandateBasis {
    pub via: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addressed: Option<Addressed>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<Observation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<String>>,
}

impl MandateBasis {
    pub fn via(via: &str) -> Self {
        Self {
            via: via.to_owned(),
            ..Self::default()
        }
    }
}

/// Immutable authorization produced by an accept decision; execution is bounded by it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mandate {
    pub strength: Strength,
    pub policy_version: String,
    pub basis: MandateBasis,
    pub actor: ActorRef,
    pub scope: String,
}

/// Active trigger evaluation on a thread; at most one exists per thread.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Evaluation {
    pub id: String,
    pub thread_key: String,
    pub trigger_ids: Vec<String>,
    /// The admissible Trigger that gave the batch its strength.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    pub strength: Strength,
    pub actor: ActorRef,
    pub basis: MandateBasis,
    pub policy_version: String,
    pub started_at: String,
    /// When the candidate reached the PM; the deadline counts from then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered: Option<String>,
    pub deadline: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_at: Option<String>,
}

/// Thread aggregate: consistency boundary of eval, holder of subscriptions and decision history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub key: String,
    #[serde(rename = "ref")]
    pub reference: ThreadRef,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_at: Option<String>,
    /// Tickets subscribed to this thread; live ones receive every trigger through fan-out.
    #[serde(default)]
    pub subscriptions: Vec<String>,
    /// The subscribed tickets whose worker follows the thread for read-only context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Vec<String>>,
    /// Keys of threads that shared a trigger with this one; context only.
    #[serde(default)]
    pub correlated: Vec<String>,
    /// Triggers waiting for the next eval.
    #[serde(default)]
    pub pending: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluating: Option<Evaluation>,
    /// How many Evals of its queued Triggers expired in a row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expired: Option<u32>,
    /// Source-native attributes; a Source may also describe the thread for people (`label`, `facts`).
    #[serde(default)]
    pub meta: Map<String, Value>,
}

impl Thread {
    pub fn new(reference: ThreadRef, created_at: &str) -> Self {
        Self {
            key: thread_key(&reference),
            reference,
            created_at: created_at.to_owned(),
            last_at: None,
            subscriptions: Vec::new(),
            context: None,
            correlated: Vec::new(),
            pending: Vec::new(),
            evaluating: None,
            expired: None,
            meta: Map::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionKind {
    Ignore,
    Accept,
    Reopen,
    Stop,
    Unavailable,
}

/// Outcome of one eval, recorded on the thread.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub id: String,
    pub eval_id: String,
    pub thread_key: String,
    pub trigger_ids: Vec<String>,
    pub policy_version: String,
    pub at: String,
    pub kind: DecisionKind,
    /// Why, by code: one reason, or one for each Trigger of the batch it concerns.
    #[serde(default)]
    pub reason: Vec<Reason>,
    pub strength: Strength,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mandate: Option<Mandate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit: Option<Map<String, Value>>,
    /// A person dismissed the problem an unavailable Decision shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssignmentState {
    Pending,
    Running,
    Ended,
}

/// Why a ticket's worker most recently changed its activity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LastActivityKind {
    TurnStarted,
    TurnEnded,
    Report,
}

/// The latest activity recorded for a ticket's worker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastActivity {
    pub at: String,
    pub kind: LastActivityKind,
    pub text: String,
}

impl LastActivity {
    pub fn turn_started(at: &str) -> Self {
        Self::new(at, LastActivityKind::TurnStarted, "")
    }

    pub fn turn_ended(at: &str) -> Self {
        Self::new(at, LastActivityKind::TurnEnded, "")
    }

    pub fn report(at: &str, summary: &str) -> Self {
        Self::new(at, LastActivityKind::Report, summary)
    }

    /// Whole minutes elapsed since this activity, clamped at zero for future timestamps.
    pub fn minutes_since(&self, now_ms: i64) -> Option<i64> {
        DateTime::parse_from_rfc3339(&self.at)
            .ok()
            .map(|at| (now_ms - at.timestamp_millis()).max(0) / 60_000)
    }

    fn new(at: &str, kind: LastActivityKind, text: &str) -> Self {
        Self {
            at: at.to_owned(),
            kind,
            text: text.chars().take(120).collect(),
        }
    }
}

/// What an Assignment reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReportState {
    #[default]
    Progress,
    Waiting,
    Ready,
    Blocked,
    Failed,
    Dropped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub state: ReportState,
    pub summary: String,
    pub at: String,
}

/// Binding between a ticket and one executor instance (an assignment of the design's §6.7).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Assignment {
    pub id: String,
    pub state: AssignmentState,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<Report>,
    /// When Inputs last reached the worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<String>,
    /// When a turn of its worker last ended with nothing left to run and nothing waiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_at: Option<String>,
    /// Which executor runs it (`codext_thread` in A1) and its reference there (the thread id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_ref: Option<String>,
    /// The execution environment its commands run in: local, or an exec-server environment id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
}

impl Assignment {
    pub fn new(id: &str, created_at: &str) -> Self {
        Self {
            id: id.to_owned(),
            state: AssignmentState::Pending,
            created_at: created_at.to_owned(),
            started_at: None,
            ended_at: None,
            ended_reason: None,
            report: None,
            delivered_at: None,
            idle_at: None,
            executor_kind: None,
            executor_ref: None,
            environment: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleChange {
    Changed,
    Disabled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleEffect {
    pub id: String,
    pub name: String,
    pub change: RuleChange,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
}

/// How a Policy change bears on a ticket.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyEffect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<RuleEffect>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proactive: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_allowed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester: Option<ActorRef>,
}

/// A field whose `null` is a value: `Some(None)` is written as `null` and must read back as
/// `Some(None)`, not as the absence `None` that plain `Option<Option<T>>` deserialization yields.
fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

/// What a ticket's worker receives, in order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TicketInput {
    /// A Trigger on one of its Threads, or attached to it.
    #[serde(rename_all = "camelCase")]
    Trigger {
        id: String,
        trigger_id: String,
        at: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        /// A person wrote it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        human: Option<bool>,
        /// It reached the ticket only on a Thread its worker follows for context.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<bool>,
        /// When the worker's thread took it into a step; `Some(None)` while it waits there.
        #[serde(
            default,
            deserialize_with = "double_option",
            skip_serializing_if = "Option::is_none"
        )]
        read_at: Option<Option<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delivered_to: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delivered_at: Option<String>,
    },
    /// The Policy changed in a way that bears on the ticket.
    #[serde(rename = "policy-changed", rename_all = "camelCase")]
    PolicyChanged {
        id: String,
        from: String,
        to: String,
        at: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effect: Option<PolicyEffect>,
    },
    /// A Decision reopened the ticket.
    #[serde(rename_all = "camelCase")]
    Reopen {
        id: String,
        decision_id: String,
        at: String,
    },
    /// A person's message on one of its Threads resumed the ended ticket under its unchanged Mandate.
    #[serde(rename_all = "camelCase")]
    Resumed {
        id: String,
        trigger_id: String,
        at: String,
    },
    Text {
        id: String,
        text: String,
        at: String,
    },
    /// A fact the PM gave the ticket from other work: context, which asks for nothing and grants nothing.
    Note {
        id: String,
        text: String,
        at: String,
    },
}

impl TicketInput {
    pub fn id(&self) -> &str {
        match self {
            Self::Trigger { id, .. }
            | Self::PolicyChanged { id, .. }
            | Self::Reopen { id, .. }
            | Self::Resumed { id, .. }
            | Self::Text { id, .. }
            | Self::Note { id, .. } => id,
        }
    }

    pub fn at(&self) -> &str {
        match self {
            Self::Trigger { at, .. }
            | Self::PolicyChanged { at, .. }
            | Self::Reopen { at, .. }
            | Self::Resumed { at, .. }
            | Self::Text { at, .. }
            | Self::Note { at, .. } => at,
        }
    }

    /// The Trigger it carries, when it carries one.
    pub fn trigger_id(&self) -> Option<&str> {
        match self {
            Self::Trigger { trigger_id, .. } => Some(trigger_id),
            _ => None,
        }
    }

    /// A person's message that reached the ticket as its own, not only as context.
    pub fn human_own(&self) -> bool {
        matches!(self, Self::Trigger { human: Some(true), context, .. } if context != &Some(true))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TicketEndBy {
    Executor,
    Human,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TicketOutcome {
    Completed,
    Merged,
    Closed,
    Dropped,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ended {
    pub at: String,
    pub by: TicketEndBy,
    pub outcome: TicketOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A question for a person; `trigger_id` is the human message that raised it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attention {
    pub id: String,
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CloseOutcome {
    Merged,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Closed {
    pub at: String,
    pub outcome: CloseOutcome,
}

/// A Thread a ticket follows: `own` when the ticket opened it, and when and how it closed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Follow {
    pub thread: String,
    pub own: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<Closed>,
}

/// What the executor chose for a ticket: its short name, directory and assignment text.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Execution {
    pub name: String,
    pub directory: String,
    pub assignment: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryFailure {
    pub error: String,
    pub at: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Origin {
    pub trigger_id: String,
    #[serde(default)]
    pub trigger_ids: Vec<String>,
}

/// Ticket aggregate: an authorized work item with its assignments, channels, thread bindings and inputs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ticket {
    pub id: String,
    pub title: String,
    pub decision_id: String,
    pub mandate: Mandate,
    #[serde(default)]
    pub channels: Vec<ChannelRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<Ended>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<Attention>,
    #[serde(default)]
    pub assignments: Vec<Assignment>,
    #[serde(default)]
    pub inputs: Vec<TicketInput>,
    /// Inputs not yet handed to a running assignment.
    #[serde(default)]
    pub undelivered: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follows: Option<Vec<Follow>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answers: Option<Vec<String>>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<LastActivity>,
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<Execution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryFailure>,
}

/// Whether a ticket is open: it has not ended.
pub fn live_ticket(ticket: &Ticket) -> bool {
    ticket.ended.is_none()
}

pub fn ticket_outcome(ticket: &Ticket) -> Option<TicketOutcome> {
    ticket.ended.as_ref().map(|ended| ended.outcome)
}

pub fn running_assignment(ticket: &Ticket) -> Option<&Assignment> {
    ticket
        .assignments
        .iter()
        .find(|assignment| assignment.state == AssignmentState::Running)
}

pub fn latest_assignment(ticket: &Ticket) -> Option<&Assignment> {
    ticket.assignments.last()
}

/// The latest report any of a ticket's Assignments made, which is what the ticket last said.
pub fn latest_report(ticket: &Ticket) -> Option<&Report> {
    ticket
        .assignments
        .iter()
        .rev()
        .find_map(|assignment| assignment.report.as_ref())
}

/// Until when a ticket's worker had a turn to read what reached it: its latest report (a progress
/// report only when `progress` is true) or the end of its latest turn, whichever is later.
pub fn read_until(ticket: &Ticket, progress: bool) -> String {
    let report_at = ticket
        .assignments
        .iter()
        .rev()
        .find_map(|assignment| match &assignment.report {
            Some(report) if progress || report.state != ReportState::Progress => {
                Some(report.at.clone())
            }
            _ => None,
        })
        .unwrap_or_default();
    ticket
        .assignments
        .iter()
        .map(|assignment| assignment.idle_at.clone().unwrap_or_default())
        .fold(report_at, |a, b| if a > b { a } else { b })
}

/// Whether a ticket waits for a person: its latest Assignment reported blocked or failed, and no
/// person's message reached it since.
pub fn waits_for_person(ticket: &Ticket) -> bool {
    let Some(report) = latest_assignment(ticket).and_then(|assignment| assignment.report.as_ref())
    else {
        return false;
    };
    if report.state != ReportState::Blocked && report.state != ReportState::Failed {
        return false;
    }
    !ticket
        .inputs
        .iter()
        .any(|input| input.human_own() && input.at() > report.at.as_str())
}

/// Whether a Trigger reached a ticket: it started the ticket, came to it as an Input, or is on a
/// Thread the ticket subscribes to; with `context` false, not a Thread followed only for context.
pub fn reached(
    ticket: &Ticket,
    trigger: &Trigger,
    thread: impl Fn(&str) -> Option<Thread>,
    context: bool,
) -> bool {
    if ticket.origin.trigger_id == trigger.id {
        return true;
    }
    let input = ticket
        .inputs
        .iter()
        .any(|item| item.trigger_id() == Some(trigger.id.as_str()));
    let follows_for_context = |current: &Thread| {
        current
            .context
            .as_ref()
            .is_some_and(|ids| ids.contains(&ticket.id))
    };
    let on_subscribed = trigger.threads.iter().any(|reference| {
        thread(&thread_key(reference)).is_some_and(|current| {
            current.subscriptions.contains(&ticket.id)
                && (context || !follows_for_context(&current))
        })
    });
    on_subscribed
        || (input
            && (context
                || !trigger.threads.iter().all(|reference| {
                    thread(&thread_key(reference))
                        .is_some_and(|current| follows_for_context(&current))
                })))
}

/// What deciding a follow-up's owners reads.
pub trait FollowupRecords {
    fn ticket(&self, id: &str) -> Option<Ticket>;
    fn thread(&self, key: &str) -> Option<Thread>;
    fn disposition(&self, trigger_id: &str) -> Option<TriggerDisposition>;
}

/// The tickets whose worker decides a person's follow-up: those it reached on their own Threads that
/// can still decide it and did not hand it back.
pub fn followup_owners(trigger: &Trigger, records: &impl FollowupRecords) -> Vec<String> {
    if !trigger.human {
        return Vec::new();
    }
    let Some(disposition) = records.disposition(&trigger.id) else {
        return Vec::new();
    };
    if disposition.requeued == Some(true) {
        return Vec::new();
    }
    let returned: Vec<&str> = disposition
        .handbacks
        .iter()
        .flatten()
        .map(|item| item.ticket_id.as_str())
        .collect();
    let context = disposition.context.clone().unwrap_or_default();
    let reached: Vec<&String> = disposition
        .fanout
        .iter()
        .filter(|id| !returned.contains(&id.as_str()) && !context.contains(id))
        .collect();
    if reached.is_empty() {
        return Vec::new();
    }
    let held_by_eval = trigger.threads.first().is_some_and(|primary| {
        records
            .thread(&thread_key(primary))
            .and_then(|thread| thread.evaluating)
            .is_some_and(|evaluation| evaluation.trigger_ids.contains(&trigger.id))
    });
    if held_by_eval {
        return Vec::new();
    }
    reached
        .into_iter()
        .filter(|id| {
            records.ticket(id).is_some_and(|ticket| {
                live_ticket(&ticket)
                    || (ticket.ended.as_ref().map(|e| e.by) != Some(TicketEndBy::Human)
                        && ticket.inputs.iter().any(|input| {
                            input.trigger_id() == Some(trigger.id.as_str())
                                && ticket.undelivered.iter().any(|u| u == input.id())
                        }))
            })
        })
        .cloned()
        .collect()
}

/// Whether a Decision settled what a person's message asks for.
pub fn settled(decision: &Decision) -> bool {
    !matches!(
        decision.kind,
        DecisionKind::Stop | DecisionKind::Unavailable
    ) && !decision.reason.iter().any(|reason| {
        matches!(
            reason.code.as_str(),
            "stopNotExplicit" | "stopUnknown" | "routed"
        )
    })
}

