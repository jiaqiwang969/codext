//! Refusals a person or an agent can cause (prototype `src/core/errors.ts`).

use std::collections::BTreeMap;

/// Whether the request named something missing, or asked for something refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Missing,
    Refused,
}

pub type UserErrorCode = &'static str;

/// Every code with its kind and English text; `{name}` is filled from the values.
pub const USER_ERRORS: &[(&str, ErrorKind, &str)] = &[
    ("unknownTicket", ErrorKind::Missing, "Unknown ticket"),
    (
        "unknownProblem",
        ErrorKind::Missing,
        "Unavailable Eval not found",
    ),
    ("unknownTrigger", ErrorKind::Missing, "Unknown trigger"),
    (
        "unknownAssignment",
        ErrorKind::Missing,
        "Unknown assignment",
    ),
    ("ticketEnded", ErrorKind::Refused, "Ticket has ended"),
    (
        "assignmentEnded",
        ErrorKind::Refused,
        "Assignment has ended; a newer assignment owns this ticket",
    ),
    (
        "evalFinished",
        ErrorKind::Refused,
        "Eval {eval} has finished or a later eval replaced it; decide the trigger's current eval",
    ),
    (
        "notAdmissible",
        ErrorKind::Refused,
        "Trigger is not admissible: {reason}",
    ),
    (
        "notExplicit",
        ErrorKind::Refused,
        "An accept through superagent_route is for explicit requests from allowed senders; assess proactive candidates with the eval tools",
    ),
    (
        "ticketStopped",
        ErrorKind::Refused,
        "A person stopped this ticket; new work needs an accept through superagent_route",
    ),
    (
        "mandateRevoked",
        ErrorKind::Refused,
        "The current policy no longer holds this ticket's mandate; new work needs an accept through superagent_route",
    ),
    (
        "notRetryable",
        ErrorKind::Refused,
        "Only an open ticket can be retried, and only when its worker last reported failed or blocked and no person has sent a message since, or when delivering an Input to its worker failed",
    ),
    (
        "sourceThread",
        ErrorKind::Refused,
        "This is the thread the ticket started on; it stays subscribed",
    ),
    (
        "followedThread",
        ErrorKind::Refused,
        "This thread is the ticket's own; it stays subscribed",
    ),
    (
        "notFollowing",
        ErrorKind::Refused,
        "This ticket does not follow that thread",
    ),
    (
        "questionWithResult",
        ErrorKind::Refused,
        "A question for a person cannot also be a ready result or an answer; report it separately",
    ),
    (
        "askerWithoutQuestion",
        ErrorKind::Refused,
        "The message that raised a question needs the question it raised",
    ),
    (
        "notAsker",
        ErrorKind::Refused,
        "A question must name the person's message that raised it, which reached this ticket",
    ),
    (
        "notAnswerable",
        ErrorKind::Refused,
        "An answer must name the person's question it answers, which reached this ticket",
    ),
    (
        "ackEnded",
        ErrorKind::Refused,
        "Ticket ended; do not acknowledge historical requests",
    ),
    (
        "notReached",
        ErrorKind::Refused,
        "Trigger does not belong to this worker",
    ),
    (
        "handedBack",
        ErrorKind::Refused,
        "This ticket handed that trigger back already",
    ),
    (
        "handedBackHere",
        ErrorKind::Refused,
        "The worker of this ticket handed that trigger back as beyond it: accept it as new work, attach it to another ticket, or ignore it",
    ),
    (
        "workerDecides",
        ErrorKind::Refused,
        "This message reached ticket {tickets} on its thread: its worker decides whether it belongs there and hands back what does not, which then arrives as a candidate. Only a stop goes through superagent_route, with kind stop",
    ),
    (
        "ackNoChannel",
        ErrorKind::Refused,
        "Acknowledgement requires an assigned human source message",
    ),
    (
        "ackNotPerson",
        ErrorKind::Refused,
        "Use the original human trigger, not a bot, CI or worker report",
    ),
    (
        "notReady",
        ErrorKind::Refused,
        "Ticket is not ready for delivery. Keep progress internal and wait for a verified ready report; do not relabel progress as an answer or send it through another channel.",
    ),
    (
        "notAttention",
        ErrorKind::Refused,
        "Attention requires the ticket's current attention question in human_action. Routine unfinished work stays internal.",
    ),
    (
        "notQuestion",
        ErrorKind::Refused,
        "An answer must target the original human question, not a worker report or automated trigger",
    ),
    (
        "noAnswer",
        ErrorKind::Refused,
        "No worker answer is ready for this human trigger. Wait for the worker's report, or request one with superagent_request_status, which acts only on a worker that reported waiting; ordinary progress cannot be relabelled as an answer.",
    ),
    (
        "noChannel",
        ErrorKind::Refused,
        "No reply channel for this ticket or trigger",
    ),
];

/// A refusal with a fixed code, so tools and the TUI can word it; the message is the English text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct UserError {
    pub code: String,
    pub kind: ErrorKind,
    pub values: BTreeMap<String, String>,
    pub message: String,
}

impl UserError {
    pub fn new(code: &str, values: &[(&str, &str)]) -> Self {
        let values: BTreeMap<String, String> = values
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let (kind, template) = USER_ERRORS
            .iter()
            .find(|(c, _, _)| *c == code)
            .map_or((ErrorKind::Refused, code), |(_, kind, text)| (*kind, *text));
        let mut message = template.to_owned();
        for (name, value) in &values {
            message = message.replace(&format!("{{{name}}}"), value);
        }
        Self {
            code: code.to_owned(),
            kind,
            values,
            message,
        }
    }

    pub fn missing(&self) -> bool {
        self.kind == ErrorKind::Missing
    }
}

