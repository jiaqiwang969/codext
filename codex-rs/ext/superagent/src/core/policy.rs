//! The A1 Policy: one source (`codext`), the user always listed and explicit, modes kept as data
//! (prototype `src/policy.ts`: `gate`, `revalidate` and `admitsInferred`, without the admission table).

use super::model::AcceptMode;
use super::model::Addressed;
use super::model::Mandate;
use super::model::MandateBasis;
use super::model::Strength;
use super::model::Thread;
use super::model::Ticket;
use super::model::Trigger;
use super::ports::GateResult;
use super::ports::Policy;

/// The Policy of the codext organisation. An immutable snapshot; a change installs a new one.
#[derive(Clone, Debug)]
pub struct CodextPolicy {
    pub version: String,
    pub paused: bool,
    pub mode: AcceptMode,
    pub eval_timeout_ms: i64,
    /// The senders who may originate work; A1 lists the one user.
    pub allowed: Vec<String>,
}

impl Default for CodextPolicy {
    fn default() -> Self {
        Self {
            version: "a1-v1".to_owned(),
            paused: false,
            mode: AcceptMode::Observe,
            eval_timeout_ms: 600_000,
            allowed: vec!["user".to_owned()],
        }
    }
}

impl CodextPolicy {
    fn allowed(&self, sender: &str) -> bool {
        self.allowed.iter().any(|id| id == sender)
    }
}

/// Whether a thread has tickets of its own, beyond the ones that follow it only for context.
fn participating(thread: &Thread, tickets: &[Ticket]) -> bool {
    let context = thread.context.clone().unwrap_or_default();
    thread
        .subscriptions
        .iter()
        .chain(tickets.iter().map(|t| &t.id))
        .any(|id| !context.contains(id))
}

impl Policy for CodextPolicy {
    fn version(&self) -> String {
        self.version.clone()
    }

    fn paused(&self) -> bool {
        self.paused
    }

    fn eval_timeout_ms(&self) -> i64 {
        self.eval_timeout_ms
    }

    fn gate(&self, trigger: &Trigger, thread: &Thread, tickets: &[Ticket]) -> GateResult {
        // Bots and CI never originate work and never stop or resume a ticket; they reach tickets only as context.
        if !trigger.human {
            return GateResult::Inadmissible {
                reason: "not_human".to_owned(),
            };
        }
        let allowed = self.allowed(&trigger.actor.id);
        let mut basis = MandateBasis::via("addressed");
        basis.source = Some(trigger.source.clone());
        if allowed {
            basis.sender = Some(trigger.actor.id.clone());
        }
        if allowed && trigger.addressed.is_some() {
            basis.addressed = trigger.addressed;
            return GateResult::Admissible {
                strength: Strength::Explicit,
                basis,
            };
        }
        // A direct message belongs to one sender: no follow-up for others.
        if !allowed && trigger.addressed == Some(Addressed::Direct) {
            return GateResult::Inadmissible {
                reason: "not_a_member".to_owned(),
            };
        }
        if (allowed || self.mode != AcceptMode::Off) && participating(thread, tickets) {
            let mut followup = MandateBasis::via("followup");
            followup.source = basis.source;
            followup.sender = basis.sender;
            let strength = if allowed {
                Strength::Explicit
            } else {
                Strength::Inferred
            };
            return GateResult::Admissible {
                strength,
                basis: followup,
            };
        }
        GateResult::Inadmissible {
            reason: if allowed {
                "no_mention".to_owned()
            } else {
                "not_a_member".to_owned()
            },
        }
    }

    fn revalidate(&self, mandate: &Mandate) -> bool {
        if mandate.basis.via == "followup" {
            return mandate.strength == Strength::Explicit || self.mode == AcceptMode::Auto;
        }
        if !self.allowed(&mandate.actor.id) {
            return false;
        }
        match &mandate.basis.observation {
            Some(_) => self.mode == AcceptMode::Auto,
            None => true,
        }
    }

    fn admits_inferred(&self, rules: &[String]) -> bool {
        self.mode == AcceptMode::Auto && !rules.is_empty()
    }
}

