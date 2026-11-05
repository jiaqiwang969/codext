//! Structured reason codes for decisions, filtered triggers and ended assignments (prototype `src/core/reasons.ts`).

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

/// The core's codes with the English an agent reads; a gate or a Source may use its own codes.
pub const REASONS: &[(&str, &str)] = &[
    ("said", "{text}"),
    ("timeout", "Assessment timed out"),
    ("policyChanged", "Policy changed during assessment"),
    ("noAdmissible", "No admissible trigger"),
    ("startedTicket", "Already started ticket {ticket}"),
    ("acceptedBefore", "Trigger {trigger} was accepted before"),
    ("routed", "Routed to ticket {ticket}"),
    (
        "explicitRouted",
        "Explicit request from an allowed sender routed by the orchestrator",
    ),
    ("humanStop", "Human asked to stop"),
    (
        "forwarded",
        "Forwarded to the worker of the Ticket already on this Thread",
    ),
    ("senderNotMember", "The sender is not on the Source's list"),
    ("retried", "Retried by a person"),
    ("hostRestarted", "codext restarted"),
    ("stopUnknown", "Stop target is unknown or already ended"),
    (
        "stopNotExplicit",
        "A stop needs an explicit request from a sender on the list",
    ),
    (
        "directionUnsettled",
        "Implementation intent or target direction is not established, or a product/design decision is still required",
    ),
    ("noRuleMatched", "No enabled admission rule matched"),
    (
        "ignoredFromCard",
        "Ignored from its card, without a rule assessment",
    ),
    ("observed", "Observe mode: would accept ({text})"),
];

/// A reason code with its template values; `trigger_id` names the Trigger of a batch it applies to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reason {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_id: Option<String>,
}

impl Reason {
    pub fn code(code: &str) -> Self {
        Self {
            code: code.to_owned(),
            ..Self::default()
        }
    }

    pub fn with(mut self, name: &str, value: &str) -> Self {
        self.values
            .get_or_insert_with(BTreeMap::new)
            .insert(name.to_owned(), value.to_owned());
        self
    }

    pub fn for_trigger(mut self, trigger_id: &str) -> Self {
        self.trigger_id = Some(trigger_id.to_owned());
        self
    }

    /// Words an assessor or a person wrote.
    pub fn said(text: &str) -> Self {
        Self::code("said").with("text", text)
    }
}

fn fill(template: &str, values: Option<&BTreeMap<String, String>>) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end)
                if after[..end]
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_') =>
            {
                let name = &after[..end];
                match values.and_then(|v| v.get(name)) {
                    Some(value) => out.push_str(value),
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Formats reasons in English, prefixed by their Trigger id when they name one, joined by `; `.
pub fn reason_text(reasons: &[Reason]) -> String {
    reasons
        .iter()
        .map(|reason| {
            let template = REASONS
                .iter()
                .find(|(code, _)| *code == reason.code)
                .map_or(reason.code.as_str(), |(_, template)| *template);
            let text = fill(template, reason.values.as_ref());
            match &reason.trigger_id {
                Some(id) => format!("{id}: {text}"),
                None => text,
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

