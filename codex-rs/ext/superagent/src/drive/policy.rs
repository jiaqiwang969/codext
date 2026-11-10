//! The knobs of the account moves: the quota thresholds, how often the pool is watched, and when
//! a Router turn stuck on its account is stopped and moved. Each has a default and an
//! environment variable, read once when the engine starts (`lib.rs`).

use std::sync::Arc;
use std::time::Duration;

use super::accounts::AccountSource;
use super::accounts::HARD_REMAINING;
use super::accounts::SOFT_REMAINING;
use super::accounts::Thresholds;

/// Below this quota (percent) a thread leaves its account between turns.
pub const SOFT_PERCENT_VAR: &str = "CODEXT_SUPERAGENT_QUOTA_SOFT_PERCENT";
/// Below this quota (percent) an account counts as spent.
pub const HARD_PERCENT_VAR: &str = "CODEXT_SUPERAGENT_QUOTA_HARD_PERCENT";
/// Seconds between two reads of the pool; 0 turns the watch off.
pub const WATCH_SECS_VAR: &str = "CODEXT_SUPERAGENT_QUOTA_WATCH_SECS";
/// Capacity or usage-limit incidents of one Router turn that stop it; 0 turns the rescue off.
pub const RESCUE_THRESHOLD_VAR: &str = "CODEXT_SUPERAGENT_ROUTER_RESCUE_THRESHOLD";
/// The window (seconds) those incidents must fall in.
pub const RESCUE_WINDOW_SECS_VAR: &str = "CODEXT_SUPERAGENT_ROUTER_RESCUE_WINDOW_SECS";

const WATCH_EVERY: Duration = Duration::from_secs(120);
const RESCUE_AFTER: usize = 2;
const RESCUE_WINDOW: Duration = Duration::from_secs(180);
/// How old the pool may be, when nothing watches it, before a check reads it again.
const POOL_MAX_AGE: Duration = Duration::from_secs(240);

/// The Router threads, looked up on each use.
pub type Routers = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

/// What the drive works with besides the store, the effects, the host and the judge.
pub struct DriveOptions {
    /// The account pool, for moving a thread whose account runs low or cannot carry a turn.
    pub accounts: Option<Arc<dyn AccountSource>>,
    /// How long a handed-over step may wait for its turn before the drive pauses.
    pub stall: Duration,
    pub policy: AccountPolicy,
    /// The Router threads whose accounts the drive watches.
    pub routers: Routers,
}

/// How the engine keeps the Router and the driven sessions on accounts with quota left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AccountPolicy {
    /// Below `soft` percent left a thread leaves its account between turns; below `hard` (or
    /// cooling down, or not usable) the account counts as spent.
    pub thresholds: Thresholds,
    /// How often the pool is read and the Routers' accounts are checked; zero turns it off.
    pub watch_every: Duration,
    /// Capacity or usage-limit incidents of one Router turn, within `rescue_window`, after which
    /// the engine stops the turn and moves the Router; zero turns the rescue off.
    pub rescue_after: usize,
    pub rescue_window: Duration,
}

impl Default for AccountPolicy {
    fn default() -> Self {
        Self {
            thresholds: Thresholds::new(SOFT_REMAINING, HARD_REMAINING),
            watch_every: WATCH_EVERY,
            rescue_after: RESCUE_AFTER,
            rescue_window: RESCUE_WINDOW,
        }
    }
}

impl AccountPolicy {
    /// The defaults with the knobs `var` answers applied; a knob that is unset, empty or not a
    /// number keeps its default, and a percentage must lie within 0..=100.
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Self {
        let value = |name: &str| {
            var(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let percent = |name: &str| {
            value(name)
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|percent| (0.0..=100.0).contains(percent))
        };
        let secs = |name: &str| {
            value(name)
                .and_then(|value| value.parse::<u64>().ok())
                .map(Duration::from_secs)
        };
        let defaults = Self::default();
        Self {
            thresholds: Thresholds::new(
                percent(SOFT_PERCENT_VAR).unwrap_or(defaults.thresholds.soft),
                percent(HARD_PERCENT_VAR).unwrap_or(defaults.thresholds.hard),
            ),
            watch_every: secs(WATCH_SECS_VAR).unwrap_or(defaults.watch_every),
            rescue_after: value(RESCUE_THRESHOLD_VAR)
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(defaults.rescue_after),
            rescue_window: secs(RESCUE_WINDOW_SECS_VAR)
                .filter(|window| !window.is_zero())
                .unwrap_or(defaults.rescue_window),
        }
    }

    /// How old the pool may be before a check between turns reads it again: two watch periods,
    /// so the watch normally keeps it fresh and the check costs nothing.
    pub fn pool_max_age(&self) -> Duration {
        if self.watch_every.is_zero() {
            POOL_MAX_AGE
        } else {
            self.watch_every.saturating_mul(2)
        }
    }
}
