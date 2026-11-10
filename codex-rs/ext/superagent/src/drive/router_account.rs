//! Keeping the Router on an account with quota left before it runs out. This is the main path;
//! the moves after a turn its account failed or kept stuck (`router_rescue.rs`) are the net.
//!
//! - The watch reads the pool every couple of minutes and checks each Router's account. An idle
//!   Router whose account is low (below the soft threshold) or spent (below the hard threshold,
//!   cooling down, not usable) moves to the best account at once. A Router in a turn is never
//!   stopped for quota: it moves when the turn ends.
//! - When a Router turn ends, and before the engine hands a Router anything, the same check runs
//!   on the pool the watch read, so the next turn starts on an account with headroom. The end of
//!   a turn that has to move holds the Router's queued input (`arbiter.rs`) until the move is
//!   done: the queue would start the next turn on the old account the moment this one ends.
//! - A Router moves at most once per turn away from any one account, so it never bounces.

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use codex_protocol::ThreadId;
use serde_json::Value;
use serde_json::json;

use super::DriveService;
use super::accounts;
use super::accounts::AccountInfo;
use super::accounts::Activity;
use super::accounts::Plan;
use super::accounts::Trouble;
use super::accounts::now;
use super::router_rescue::ROUTER_MOVED_KEY;
use crate::effects::EffectError;
use crate::effects::Effects;
use crate::effects::HandlerOptions;
use crate::host::DeliveryGate;

/// The effect kind that looks at a Router's account when one of its turns ended, keyed
/// `<thread>#<turn>` (rows of earlier versions are keyed by the thread alone).
pub const KIND_ROUTER: &str = "drive-router-account";
/// The effect kind of the watch, one row that schedules itself again.
pub const KIND_WATCH: &str = "drive-account-watch";
const WATCH_KEY: &str = "pool";
/// The ledger key of the turns after which a Router moved by itself, as `<turn>@<account left>`.
const ROUTER_LEFT_KEY: &str = "drive-router-left";
/// How long a Router's queued input may wait for the move at the end of its turn.
const HOLD: Duration = Duration::from_secs(60);
/// How many of a Router's latest turns are searched for the one that ended.
pub(super) const RECENT_TURNS: u32 = 5;
/// The end of a turn reaches its history a moment after the turn's hook: look again this often,
/// this many times.
const SETTLE_PAUSE: Duration = Duration::from_millis(250);
const SETTLE_TRIES: u32 = 20;

impl DriveService {
    pub(super) fn is_router(&self, thread_id: &str) -> bool {
        (self.inner.routers)().iter().any(|id| id == thread_id)
    }

    /// A Router turn started (its start hook).
    pub fn router_turn_started(&self, thread_id: &str, turn_id: &str) {
        let mut watch = self.watch();
        watch.turns.insert(thread_id.to_owned(), turn_id.to_owned());
        watch.troubled.remove(thread_id);
    }

    /// The Router's account failed its turn (its error hook): the end of the turn holds the queue
    /// for the move.
    pub fn router_turn_troubled(&self, thread_id: &str, turn_id: &str, trouble: Trouble) {
        tracing::info!(
            "superagent drive: turn {turn_id} of the Router {thread_id} failed on its account ({})",
            trouble.describe()
        );
        self.watch()
            .troubled
            .insert(thread_id.to_owned(), turn_id.to_owned());
    }

    /// A Router turn ended (its stop hook, before the queue may start the next one): when its
    /// account failed it or runs low, the Router's queued input waits for the move; the move
    /// itself runs soon, outside the hook.
    pub async fn router_turn_ended(&self, thread_id: &str) -> anyhow::Result<()> {
        let (turn, troubled) = {
            let mut watch = self.watch();
            let turn = watch.turns.remove(thread_id);
            let troubled = watch.troubled.remove(thread_id);
            let troubled = troubled.is_some() && troubled == turn;
            (turn, troubled)
        };
        if troubled || self.move_due(thread_id).await {
            self.hold_queue(thread_id);
        }
        self.look_after(thread_id, turn).await
    }

    /// A Router turn was stopped (the person, or the rescue): its account is looked at soon. The
    /// queue does not start anything after a stop, so nothing is held.
    pub async fn router_turn_aborted(&self, thread_id: &str) -> anyhow::Result<()> {
        let turn = {
            let mut watch = self.watch();
            watch.troubled.remove(thread_id);
            watch.turns.remove(thread_id)
        };
        self.look_after(thread_id, turn).await
    }

    /// Asks for the look at the Router's account after `turn`, one per turn, so a second turn's
    /// end never replaces the look at the first.
    async fn look_after(&self, thread_id: &str, turn: Option<String>) -> anyhow::Result<()> {
        let key = match &turn {
            Some(turn) => format!("{thread_id}#{turn}"),
            None => thread_id.to_owned(),
        };
        self.inner
            .effects
            .enqueue_now(KIND_ROUTER, &key, &json!({ "turn": turn }))
            .await
    }

    /// Whether the Router's queued input waits for an account move (`arbiter.rs`).
    pub fn holds_queue(&self, thread_id: &str) -> bool {
        self.watch()
            .held
            .get(thread_id)
            .is_some_and(|until| *until > Instant::now())
    }

    /// The Router's queued input waits (at most `HOLD`) until `release_queue`.
    pub(super) fn hold_queue(&self, thread_id: &str) {
        self.watch()
            .held
            .insert(thread_id.to_owned(), Instant::now() + HOLD);
    }

    /// Lets the Router's queued input go: what the queue gave up while held starts now, on the
    /// account the Router has now.
    pub(super) async fn release_queue(&self, thread: ThreadId) {
        let thread_id = thread.to_string();
        let held = self.watch().held.remove(&thread_id).is_some();
        if held && let Err(err) = self.inner.host.start_queued(thread).await {
            tracing::warn!(
                "superagent drive: the queued input of the Router {thread_id} did not start: {err:#}"
            );
        }
    }

    /// The quick look while a turn ends: whether the pool as last read says the Router must
    /// move before its next turn.
    async fn move_due(&self, thread_id: &str) -> bool {
        let pool = self.cached_pool();
        if pool.is_empty() {
            return false;
        }
        let Ok(thread) = ThreadId::from_string(thread_id) else {
            return false;
        };
        let Ok(Some(current)) = self.inner.host.thread_profile(thread).await else {
            return false;
        };
        let Ok(cooling) = self.cooldowns().await else {
            return false;
        };
        let thresholds = self.inner.policy.thresholds;
        matches!(
            accounts::plan(&pool, &cooling, now(), &current, thresholds, Activity::Idle),
            Plan::Move { .. }
        )
    }

    /// A Router turn ended: when its account failed it, the safety net moves the Router and hands
    /// it the words again; then, when the account runs low, the Router moves before its next
    /// turn. Whatever the end of the turn held is let go after.
    pub async fn router_turn_boundary(
        &self,
        thread_id: &str,
        turn: Option<&str>,
    ) -> anyhow::Result<()> {
        let thread = ThreadId::from_string(thread_id)?;
        if let Some(turn) = turn
            && !self.turn_settled(thread, turn).await?
        {
            anyhow::bail!("the end of turn {turn} has not reached the Router's history yet");
        }
        let moved = async {
            self.router_turn_failed_at(thread_id, turn).await?;
            let pool = self.recent_pool().await;
            self.check_router(thread_id, &pool).await
        }
        .await;
        self.release_queue(thread).await;
        moved
    }

    /// Whether turn `turn` shows as ended in the Router's history, waiting a little for it. A
    /// turn the history does not show counts as ended.
    async fn turn_settled(&self, thread: ThreadId, turn: &str) -> anyhow::Result<bool> {
        for attempt in 0..SETTLE_TRIES {
            let turns = self.inner.host.session_turns(thread, RECENT_TURNS).await?;
            let running = turns
                .iter()
                .any(|brief| brief.id == turn && brief.status == "inProgress");
            if !running {
                return Ok(true);
            }
            if attempt + 1 < SETTLE_TRIES {
                tokio::time::sleep(SETTLE_PAUSE).await;
            }
        }
        Ok(false)
    }

    /// The Router's account against the pool: an idle Router on a low or spent account moves to
    /// the best account now, a Router in a turn moves when the turn ends, a Router no account can
    /// take over stays (warned once). A turn its account failed is left to the safety net.
    async fn check_router(&self, thread_id: &str, pool: &[AccountInfo]) -> anyhow::Result<()> {
        if pool.is_empty() {
            return Ok(());
        }
        let thread = ThreadId::from_string(thread_id)?;
        let Some(current) = self.inner.host.thread_profile(thread).await? else {
            return Ok(());
        };
        let latest = self
            .inner
            .host
            .session_turns(thread, 1)
            .await?
            .into_iter()
            .next();
        let activity = match &latest {
            Some(turn) if turn.status == "inProgress" => Activity::InTurn,
            Some(_) | None => Activity::Idle,
        };
        if activity == Activity::Idle
            && let Some(turn) = &latest
            && accounts::trouble(turn).is_some()
            && !self.claimed(ROUTER_MOVED_KEY, &turn.id).await?
        {
            return Ok(());
        }
        let cooling = self.cooldowns().await?;
        let now = now();
        let thresholds = self.inner.policy.thresholds;
        match accounts::plan(pool, &cooling, now, &current, thresholds, activity) {
            Plan::Stay => {
                self.watch().pending.remove(thread_id);
                self.unstuck(thread_id);
            }
            Plan::AtTurnEnd { to, headroom } => {
                self.unstuck(thread_id);
                let first = self
                    .watch()
                    .pending
                    .insert(thread_id.to_owned(), to.name)
                    .is_none();
                if first {
                    tracing::info!(
                        "superagent drive: the Router {thread_id} leaves {current} ({headroom:?}) when its turn ends"
                    );
                }
            }
            Plan::NoneFree(headroom) => {
                self.watch().pending.remove(thread_id);
                self.stuck(thread_id, &current, headroom);
            }
            Plan::Move { to, .. } => {
                let after = latest.as_ref().map_or(thread_id, |turn| turn.id.as_str());
                let key = format!("{after}@{current}");
                if !self.claim(ROUTER_LEFT_KEY, &key).await? {
                    return Ok(());
                }
                if let Err(err) = self.inner.host.bind_profile(thread, to.name.clone()).await {
                    self.release_claim(ROUTER_LEFT_KEY, &key).await?;
                    return Err(err);
                }
                self.watch().pending.remove(thread_id);
                self.unstuck(thread_id);
                let why = accounts::shortfall(pool, &cooling, now, &current);
                self.note_move(
                    thread_id,
                    Some(&current),
                    &to.name,
                    &format!("Router 原账号{why}，提前换号"),
                )
                .await;
            }
        }
        Ok(())
    }

    /// One look of the watch: reads the pool and checks every Router's account.
    pub async fn watch_accounts(&self) {
        let Ok(pool) = self.live_pool().await else {
            // logged once by the read
            return;
        };
        for router in (self.inner.routers)() {
            if let Err(err) = self.check_router(&router, &pool).await {
                tracing::warn!("superagent drive: the account of the Router {router}: {err:#}");
            }
        }
    }

    /// Starts the watch (one effect row that schedules itself again); off without the account
    /// status helper or with a zero period.
    pub async fn start_account_watch(&self) -> anyhow::Result<()> {
        if self.inner.accounts.is_none() || self.inner.policy.watch_every.is_zero() {
            return Ok(());
        }
        self.inner
            .effects
            .enqueue_now(KIND_WATCH, WATCH_KEY, &json!({}))
            .await
    }

    /// Before the engine hands a Router anything: a Router whose account runs low moves first,
    /// so what it is handed starts its next turn on an account with headroom. Other threads pass
    /// at once.
    pub async fn before_router_delivery(&self, thread: ThreadId) {
        let thread_id = thread.to_string();
        if self.inner.accounts.is_none() || !self.is_router(&thread_id) {
            return;
        }
        // A cold Router reports its account once loaded; the delivery loads it anyway.
        if let Err(err) = self.inner.host.resume_thread(thread, None).await {
            tracing::warn!("superagent drive: the Router {thread_id} did not load: {err:#}");
            return;
        }
        let pool = self.recent_pool().await;
        if let Err(err) = self.check_router(&thread_id, &pool).await {
            tracing::warn!("superagent drive: the account of the Router {thread_id}: {err:#}");
        }
    }

    /// `before_router_delivery` as the host's delivery gate (`LedgerHost`).
    pub fn delivery_gate(&self) -> DeliveryGate {
        let inner = Arc::downgrade(&self.inner);
        Arc::new(move |thread| {
            let inner = inner.clone();
            Box::pin(async move {
                if let Some(inner) = inner.upgrade() {
                    DriveService { inner }.before_router_delivery(thread).await;
                }
            })
        })
    }
}

/// Registers the handlers of the Router's account effects: the look at the end of a turn, the
/// watch and the rescue.
pub(super) async fn install_router_handlers(
    service: &DriveService,
    effects: &Effects,
) -> anyhow::Result<()> {
    let mover = service.clone();
    effects
        .handle(
            KIND_ROUTER,
            Arc::new(move |payload: Value, key: String| {
                let mover = mover.clone();
                Box::pin(async move {
                    let thread_id = key
                        .split_once('#')
                        .map_or(key.as_str(), |(thread, _)| thread);
                    let turn = payload.get("turn").and_then(Value::as_str);
                    mover
                        .router_turn_boundary(thread_id, turn)
                        .await
                        .map_err(|err| EffectError::Later(format!("{err:#}")))
                })
            }),
            HandlerOptions::default().retry(10_000, 120_000),
        )
        .await?;
    let watcher = service.clone();
    effects
        .handle(
            KIND_WATCH,
            Arc::new(move |_payload, _key| {
                let watcher = watcher.clone();
                Box::pin(async move {
                    // One look, then the next one is due a period later; a period of zero (or
                    // no account status helper) ends the watch.
                    let every = watcher.inner.policy.watch_every;
                    if watcher.inner.accounts.is_none() || every.is_zero() {
                        return Ok(());
                    }
                    watcher.watch_accounts().await;
                    if let Err(err) = watcher
                        .inner
                        .effects
                        .enqueue_after(KIND_WATCH, WATCH_KEY, &json!({}), every)
                        .await
                    {
                        tracing::warn!(
                            "superagent drive: the next look of the account watch: {err:#}"
                        );
                    }
                    Ok(())
                })
            }),
            HandlerOptions::default().quiet(),
        )
        .await?;
    let rescuer = service.clone();
    effects
        .handle(
            super::router_rescue::KIND_RESCUE,
            Arc::new(move |payload: Value, thread_id| {
                let rescuer = rescuer.clone();
                Box::pin(async move {
                    let turn = payload.get("turn").and_then(Value::as_str);
                    let trouble = payload
                        .get("trouble")
                        .and_then(Value::as_str)
                        .and_then(Trouble::from_kind);
                    let (Some(turn), Some(trouble)) = (turn, trouble) else {
                        return Ok(());
                    };
                    rescuer
                        .rescue_router(&thread_id, turn, trouble)
                        .await
                        .map_err(|err| EffectError::Failed(format!("{err:#}")))
                })
            }),
            HandlerOptions::default().retry(5_000, 120_000).quiet(),
        )
        .await
}
