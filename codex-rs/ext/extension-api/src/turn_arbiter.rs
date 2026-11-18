//! Lets one owner drive a thread's automatic turns while other drivers stand down.

use crate::ExtensionData;
use crate::ExtensionFuture;

/// What an arbiter sees about an automatic turn start.
#[derive(Clone, Copy)]
pub struct AutomaticTurnInput<'a> {
    /// The thread's extension store; its `level_id()` is the thread id.
    pub thread_store: &'a ExtensionData,
    /// The start's source classification (`goal`, `queue`, `daemon_recovery`, ...), if any.
    pub turn_trigger: Option<&'a str>,
}

/// Decides whether an automatic turn may start on a thread.
///
/// Core consults every registered arbiter before an automatic start (start-if-idle or
/// continue-if-idle) consumes any input. Explicit client input and steering are not automatic and
/// never reach arbiters. An implementation answers `false` when another driver owns the thread's
/// automatic work; Core then reports the start as not submitted (`DeclinedByDriver`) and the
/// caller keeps its input. Implementations should answer from cheap state and must not start
/// turns from this callback.
pub trait AutomaticTurnArbiter: Send + Sync {
    /// Whether this automatic start may proceed.
    fn admit_automatic_turn<'a>(
        &'a self,
        input: AutomaticTurnInput<'a>,
    ) -> ExtensionFuture<'a, bool>;
}
