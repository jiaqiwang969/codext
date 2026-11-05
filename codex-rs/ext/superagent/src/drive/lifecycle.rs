//! Feeds the turns of driven sessions to the drive loop: a start clears the step in flight, an
//! end schedules the judgment, an interruption by the person pauses the drive.

use std::sync::Arc;

use codex_extension_api::ExtensionFuture;
use codex_extension_api::TurnAbortInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStartInput;
use codex_extension_api::TurnStopInput;
use codex_protocol::protocol::TurnAbortReason;

use super::DriveService;

type Lookup = Arc<dyn Fn() -> Option<DriveService> + Send + Sync>;

pub struct DriveLifecycle {
    drive: Lookup,
}

impl DriveLifecycle {
    pub fn new(drive: Lookup) -> Self {
        Self { drive }
    }
}

impl TurnLifecycleContributor for DriveLifecycle {
    fn on_turn_start<'a>(&'a self, input: TurnStartInput<'a>) -> ExtensionFuture<'a, ()> {
        let drive = (self.drive)();
        let thread_id = input.thread_store.level_id().to_owned();
        Box::pin(async move {
            if let Some(drive) = drive
                && let Err(err) = drive.turn_started(&thread_id).await
            {
                tracing::warn!("superagent drive: turn start of {thread_id}: {err:#}");
            }
        })
    }

    fn on_turn_stop<'a>(&'a self, input: TurnStopInput<'a>) -> ExtensionFuture<'a, ()> {
        let drive = (self.drive)();
        let thread_id = input.thread_store.level_id().to_owned();
        Box::pin(async move {
            if let Some(drive) = drive
                && let Err(err) = drive.turn_stopped(&thread_id).await
            {
                tracing::warn!("superagent drive: turn end of {thread_id}: {err:#}");
            }
        })
    }

    fn on_turn_abort<'a>(&'a self, input: TurnAbortInput<'a>) -> ExtensionFuture<'a, ()> {
        let drive = (self.drive)();
        let thread_id = input.thread_store.level_id().to_owned();
        let reason = input.reason;
        Box::pin(async move {
            let Some(drive) = drive else {
                return;
            };
            let result = match reason {
                TurnAbortReason::Interrupted => drive.turn_interrupted(&thread_id).await,
                // a replacing turn or a budget stop ends in its own judgment
                TurnAbortReason::Replaced
                | TurnAbortReason::ReviewEnded
                | TurnAbortReason::BudgetLimited => drive.turn_stopped(&thread_id).await,
            };
            if let Err(err) = result {
                tracing::warn!("superagent drive: turn abort of {thread_id}: {err:#}");
            }
        })
    }
}
