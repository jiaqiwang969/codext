//! Keeps Codext's own automatic drivers off the threads the organization drives, and holds a
//! Router's queued input while the drive moves the Router to another account at the end of a
//! turn (`drive/router_account.rs`), so the next turn starts on the new account.

use std::sync::Arc;
use std::sync::Weak;

use codex_extension_api::AutomaticTurnArbiter;
use codex_extension_api::AutomaticTurnInput;
use codex_extension_api::ExtensionFuture;

use crate::SuperagentService;

/// Triggers whose turns the engine decides itself on its threads: a Goal would continue a worker
/// beside the executor, and a daemon restore would continue it beside the startup reconciliation.
/// Queue starts stay allowed: the engine delivers through the queue, and a person's queued
/// message is a person's input.
const ENGINE_DECIDES: &[&str] = &["goal", "daemon_recovery"];
/// The trigger of a start the persistent queue makes when a turn ends.
const QUEUE: &str = "queue";

pub struct SuperagentArbiter {
    service: Weak<SuperagentService>,
}

impl SuperagentArbiter {
    pub fn new(service: &Arc<SuperagentService>) -> Self {
        Self {
            service: Arc::downgrade(service),
        }
    }
}

impl AutomaticTurnArbiter for SuperagentArbiter {
    fn admit_automatic_turn<'a>(
        &'a self,
        input: AutomaticTurnInput<'a>,
    ) -> ExtensionFuture<'a, bool> {
        Box::pin(async move {
            let Some(trigger) = input.turn_trigger else {
                return true;
            };
            let Some(service) = self.service.upgrade() else {
                return true;
            };
            let thread_id = input.thread_store.level_id();
            if trigger == QUEUE && service.holds_queue(thread_id) {
                tracing::info!(
                    "superagent: the queued input of {thread_id} waits for its account move"
                );
                return false;
            }
            if !ENGINE_DECIDES.contains(&trigger) {
                return true;
            }
            if service.owns_thread(thread_id).await {
                tracing::info!(
                    "superagent: declined a {trigger} turn on {thread_id}; the engine drives this thread"
                );
                return false;
            }
            true
        })
    }
}
