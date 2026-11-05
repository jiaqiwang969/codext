//! What happens when a worker's turn ends (prototype `src/executor/background.ts` and docs/003):
//! a turn that ends with background commands keeps its slot and is not reported waiting; they have
//! at most the limit to finish, then they are killed and the ticket is reported waiting; a handoff
//! to a person frees the slot at once; the end of a ticket kills its commands and its subagents.

use std::time::Duration;

use crate::core::ReportState;

pub const DEFAULT_BACKGROUND_LIMIT: Duration = Duration::from_secs(30 * 60);

/// What the executor does after a turn, given what the turn reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnEnd {
    /// Background commands still run: keep the slot and watch them until the limit.
    KeepSlot,
    /// Nothing runs and nothing was reported: report waiting and free the slot.
    ReportWaiting,
    /// The worker handed off to a person (ready, a question, blocked or failed): free the slot.
    Handoff,
    /// A progress report ended the turn: free the slot, the worker idles until the next input.
    Idle,
}

pub fn after_turn(
    reported: Option<ReportState>,
    question: bool,
    background_running: bool,
) -> TurnEnd {
    match reported {
        Some(
            ReportState::Ready | ReportState::Blocked | ReportState::Failed | ReportState::Dropped,
        ) => TurnEnd::Handoff,
        _ if question => TurnEnd::Handoff,
        _ if background_running => TurnEnd::KeepSlot,
        None => TurnEnd::ReportWaiting,
        Some(_) => TurnEnd::Idle,
    }
}

