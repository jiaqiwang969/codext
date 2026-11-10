//! Feeds the turns of driven sessions to the drive loop: a start clears the step in flight, an
//! end schedules the judgment, an interruption by the person pauses the drive. A Router's turns
//! feed its account checks instead (`router_account.rs`, `router_rescue.rs`).

use std::sync::Arc;

use codex_extension_api::ExtensionFuture;
use codex_extension_api::TurnAbortInput;
use codex_extension_api::TurnErrorInput;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStartInput;
use codex_extension_api::TurnStopInput;
use codex_protocol::protocol::TurnAbortReason;

use super::DriveService;
use super::router_rescue::error_trouble;

type Lookup = Arc<dyn Fn() -> Option<DriveService> + Send + Sync>;
/// Whether a thread is a Router (PM) thread.
type IsRouter = Arc<dyn Fn(&str) -> bool + Send + Sync>;

pub struct DriveLifecycle {
    drive: Lookup,
    is_router: IsRouter,
}

impl DriveLifecycle {
    pub fn new(drive: Lookup, is_router: IsRouter) -> Self {
        Self { drive, is_router }
    }
}

impl TurnLifecycleContributor for DriveLifecycle {
    fn on_turn_start<'a>(&'a self, input: TurnStartInput<'a>) -> ExtensionFuture<'a, ()> {
        let drive = (self.drive)();
        let thread_id = input.thread_store.level_id().to_owned();
        let router = (self.is_router)(&thread_id);
        let turn_id = input.turn_id.to_owned();
        Box::pin(async move {
            let Some(drive) = drive else {
                return;
            };
            if router {
                drive.router_turn_started(&thread_id, &turn_id);
            }
            if let Err(err) = drive.turn_started(&thread_id).await {
                tracing::warn!("superagent drive: turn start of {thread_id}: {err:#}");
            }
        })
    }

    fn on_turn_stop<'a>(&'a self, input: TurnStopInput<'a>) -> ExtensionFuture<'a, ()> {
        let drive = (self.drive)();
        let thread_id = input.thread_store.level_id().to_owned();
        let router = (self.is_router)(&thread_id);
        Box::pin(async move {
            let Some(drive) = drive else {
                return;
            };
            // The end of a Router turn looks at its account (`router_account.rs`).
            let result = if router {
                drive.router_turn_ended(&thread_id).await
            } else {
                drive.turn_stopped(&thread_id).await
            };
            if let Err(err) = result {
                tracing::warn!("superagent drive: turn end of {thread_id}: {err:#}");
            }
        })
    }

    fn on_turn_abort<'a>(&'a self, input: TurnAbortInput<'a>) -> ExtensionFuture<'a, ()> {
        let drive = (self.drive)();
        let thread_id = input.thread_store.level_id().to_owned();
        let router = (self.is_router)(&thread_id);
        let reason = input.reason;
        Box::pin(async move {
            let Some(drive) = drive else {
                return;
            };
            let result = if router {
                drive.router_turn_aborted(&thread_id).await
            } else {
                match reason {
                    TurnAbortReason::Interrupted => drive.turn_interrupted(&thread_id).await,
                    // a replacing turn or a budget stop ends in its own judgment
                    TurnAbortReason::Replaced
                    | TurnAbortReason::ReviewEnded
                    | TurnAbortReason::BudgetLimited => drive.turn_stopped(&thread_id).await,
                }
            };
            if let Err(err) = result {
                tracing::warn!("superagent drive: turn abort of {thread_id}: {err:#}");
            }
        })
    }

    fn on_turn_error<'a>(&'a self, input: TurnErrorInput<'a>) -> ExtensionFuture<'a, ()> {
        let drive = (self.drive)();
        let thread_id = input.thread_store.level_id().to_owned();
        let router = (self.is_router)(&thread_id);
        let turn_id = input.turn_id.to_owned();
        let trouble = error_trouble(&input.error, &input.error_details.to_string());
        Box::pin(async move {
            if router
                && let Some(drive) = drive
                && let Some(trouble) = trouble
            {
                drive.router_turn_troubled(&thread_id, &turn_id, trouble);
            }
        })
    }
}
