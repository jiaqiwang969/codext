//! The safety net under the account watch (`router_account.rs`): a Router turn its account could
//! not carry. When the turn failed (quota used up, model at capacity), or when codex keeps
//! retrying it inside the turn ("Reconnecting… Selected model is at capacity") so that it never
//! ends, the engine stops the turn, lets the account cool down, moves the Router to the next
//! account and hands it the turn's words again with a line on the switch. Each turn moves the
//! Router once, whichever path sees it first; with no account free nothing is stopped or moved.

use std::sync::Arc;
use std::time::Duration;

use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::protocol::CodexErrorInfo;
use serde_json::json;

use super::AccountPolicy;
use super::DriveService;
use super::accounts;
use super::accounts::Trouble;
use super::accounts::now;
use super::router_account::RECENT_TURNS;
use crate::host::DeliveryMode;
use crate::host::TurnBrief;
use crate::incidents;
use crate::incidents::Incident;

/// The effect kind that stops a Router turn stuck on its account and moves the Router.
pub const KIND_RESCUE: &str = "drive-router-rescue";
/// The ledger key of the Router turns already moved, so one turn moves the Router once.
pub(super) const ROUTER_MOVED_KEY: &str = "drive-router-moved";
/// How long a stopped turn may take to show as stopped.
const STOP_PAUSE: Duration = Duration::from_millis(250);
const STOP_TRIES: u32 = 60;

/// How the turn's words come back to the Router.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Handed {
    /// The turn failed on its account.
    AfterFailure,
    /// The turn kept retrying on its account and the engine stopped it.
    AfterRescue,
}

/// The account trouble a turn's error reports, if the error is about the account.
pub fn error_trouble(error: &CodexErrorInfo, details: &str) -> Option<Trouble> {
    if matches!(error, CodexErrorInfo::UsageLimitExceeded) {
        Some(Trouble::UsageLimit)
    } else if matches!(error, CodexErrorInfo::ServerOverloaded) {
        Some(Trouble::Capacity)
    } else {
        incidents::classify(details, /*details*/ None).and_then(Trouble::from_kind)
    }
}

/// The trouble of a Router turn stuck on its account: at least `policy.rescue_after` capacity or
/// usage-limit incidents of turn `turn` within `policy.rescue_window` before `now` (rate limits
/// pass by themselves and do not count). A usage limit among them makes it one.
pub fn rescue_due(
    recent: &[Incident],
    thread: &str,
    turn: &str,
    policy: &AccountPolicy,
    now: DateTime<Utc>,
) -> Option<Trouble> {
    if policy.rescue_after == 0 {
        return None;
    }
    let since = now - chrono::Duration::from_std(policy.rescue_window).ok()?;
    let troubles: Vec<Trouble> = recent
        .iter()
        .filter(|incident| incident.thread == thread && incident.turn.as_deref() == Some(turn))
        .filter(|incident| DateTime::parse_from_rfc3339(&incident.at).is_ok_and(|at| at >= since))
        .filter_map(|incident| Trouble::from_kind(&incident.kind))
        .collect();
    if troubles.len() < policy.rescue_after {
        return None;
    }
    Some(if troubles.contains(&Trouble::UsageLimit) {
        Trouble::UsageLimit
    } else {
        Trouble::Capacity
    })
}

impl DriveService {
    /// The Router's last turn failed on its account: the safety net moves it (`router_turn_boundary`
    /// runs this first when a turn ends).
    pub async fn router_turn_failed(&self, thread_id: &str) -> anyhow::Result<()> {
        self.router_turn_failed_at(thread_id, /*turn*/ None).await
    }

    /// Turn `turn` of the Router (the latest when `None`) failed on its account: the account
    /// cools down, the Router moves to the next account by reset time, and what the turn was
    /// about (the person's words or an engine notice) is handed to it again with a line on the
    /// switch.
    pub(super) async fn router_turn_failed_at(
        &self,
        thread_id: &str,
        turn: Option<&str>,
    ) -> anyhow::Result<()> {
        if self.inner.accounts.is_none() {
            return Ok(());
        }
        let thread = ThreadId::from_string(thread_id)?;
        let turns = self.inner.host.session_turns(thread, RECENT_TURNS).await?;
        let failed = match turn {
            Some(turn) => turns.into_iter().find(|brief| brief.id == turn),
            None => turns.into_iter().next(),
        };
        let Some(failed) = failed else {
            return Ok(());
        };
        if failed.status == "inProgress" {
            return Ok(());
        }
        // The history may show the turn interrupted and without its error for a while; the
        // error hook said what it was.
        let Some(trouble) =
            accounts::trouble(&failed).or_else(|| self.reported_trouble(thread_id, &failed.id))
        else {
            return Ok(());
        };
        if !self.claim(ROUTER_MOVED_KEY, &failed.id).await? {
            return Ok(());
        }
        self.move_router(thread, &failed, trouble, Handed::AfterFailure)
            .await
    }

    /// The listener of the model incidents (`incidents.rs`): when a Router turn has run into its
    /// account often enough within the window, the rescue is asked for (an effect, so it runs
    /// once per Router at a time and survives a restart).
    pub fn note_incident(&self, incident: &Incident) {
        let policy = self.inner.policy;
        if policy.rescue_after == 0 || self.inner.accounts.is_none() {
            return;
        }
        let Some(turn) = incident.turn.clone() else {
            return;
        };
        if Trouble::from_kind(&incident.kind).is_none() || !self.is_router(&incident.thread) {
            return;
        }
        let Some(trouble) = rescue_due(
            &incidents::recent(),
            &incident.thread,
            &turn,
            &policy,
            Utc::now(),
        ) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let drive = self.clone();
        let thread = incident.thread.clone();
        runtime.spawn(async move {
            let payload = json!({ "turn": turn, "trouble": trouble.kind() });
            if let Err(err) = drive
                .inner
                .effects
                .enqueue_now(KIND_RESCUE, &thread, &payload)
                .await
            {
                tracing::warn!("superagent drive: the rescue of the Router {thread}: {err:#}");
            }
        });
    }

    /// `note_incident` as an incident listener, holding the drive weakly.
    pub fn incident_listener(&self) -> incidents::Listener {
        let inner = Arc::downgrade(&self.inner);
        Arc::new(move |incident: &Incident| {
            if let Some(inner) = inner.upgrade() {
                DriveService { inner }.note_incident(incident);
            }
        })
    }

    /// The incidents the rescue needs recorded: the Routers'.
    pub fn incident_scope(&self) -> incidents::Scope {
        let routers = Arc::clone(&self.inner.routers);
        Arc::new(move |thread: &str| routers().iter().any(|id| id == thread))
    }

    /// Turn `turn_id` of a Router kept running into its account: unless no other account can take
    /// over (then it keeps retrying, with a warning), the turn is stopped and the Router moves as
    /// after a failed turn, getting the turn's words again. A turn that got through, or that the
    /// person has moved on from, is left alone.
    pub async fn rescue_router(
        &self,
        thread_id: &str,
        turn_id: &str,
        trouble: Trouble,
    ) -> anyhow::Result<()> {
        if self.inner.accounts.is_none() || self.claimed(ROUTER_MOVED_KEY, turn_id).await? {
            return Ok(());
        }
        let thread = ThreadId::from_string(thread_id)?;
        let turns = self.inner.host.session_turns(thread, RECENT_TURNS).await?;
        let Some(turn) = turns.first().filter(|latest| latest.id == turn_id) else {
            return Ok(());
        };
        match turn.status.as_str() {
            "completed" => return Ok(()),
            "inProgress" => {
                if !self.router_can_move(thread_id, thread).await? {
                    return Ok(());
                }
                if !self.claim(ROUTER_MOVED_KEY, turn_id).await? {
                    return Ok(());
                }
                tracing::warn!(
                    "superagent drive: turn {turn_id} of the Router {thread_id} keeps retrying ({}); stopping it to move the Router",
                    trouble.describe()
                );
                if let Err(err) = self.inner.host.interrupt(thread).await {
                    self.release_claim(ROUTER_MOVED_KEY, turn_id).await?;
                    return Err(err);
                }
            }
            // stopped already (interrupted, failed): the move is all that is left
            _ => {
                if !self.claim(ROUTER_MOVED_KEY, turn_id).await? {
                    return Ok(());
                }
            }
        }
        let Some(stopped) = self.stopped_turn(thread, turn_id).await? else {
            self.release_claim(ROUTER_MOVED_KEY, turn_id).await?;
            anyhow::bail!("turn {turn_id} of the Router {thread_id} did not stop");
        };
        // The new account's runtime wakes the queue: its next item waits until the stopped
        // turn's words are back with the Router.
        self.hold_queue(thread_id);
        let moved = self
            .move_router(thread, &stopped, trouble, Handed::AfterRescue)
            .await;
        self.release_queue(thread).await;
        moved
    }

    /// Whether another account could take the Router over now; warned once when none can.
    async fn router_can_move(&self, thread_id: &str, thread: ThreadId) -> anyhow::Result<bool> {
        let current = self.inner.host.thread_profile(thread).await.unwrap_or(None);
        let pool = self.recent_pool().await;
        let cooling = self.cooldowns().await?;
        let thresholds = self.inner.policy.thresholds;
        let free = accounts::pick(&pool, &cooling, now(), current.as_deref(), thresholds).is_some();
        if free {
            self.unstuck(thread_id);
        } else {
            self.stuck(
                thread_id,
                current.as_deref().unwrap_or("-"),
                accounts::Headroom::Spent,
            );
        }
        Ok(free)
    }

    /// Turn `turn_id` once it shows as stopped; `None` when it still runs after a while.
    async fn stopped_turn(
        &self,
        thread: ThreadId,
        turn_id: &str,
    ) -> anyhow::Result<Option<TurnBrief>> {
        for attempt in 0..STOP_TRIES {
            let turns = self.inner.host.session_turns(thread, RECENT_TURNS).await?;
            match turns.into_iter().find(|brief| brief.id == turn_id) {
                Some(turn) if turn.status == "inProgress" => {}
                Some(turn) => return Ok(Some(turn)),
                None => return Ok(None),
            }
            if attempt + 1 < STOP_TRIES {
                tokio::time::sleep(STOP_PAUSE).await;
            }
        }
        Ok(None)
    }

    /// The move after `turn` could not run on the Router's account: the account cools down
    /// (until its quota resets, or a while for a model at capacity), the Router moves to the next
    /// account and gets the turn's words again with a line on the switch. The caller claimed the
    /// turn; a move that fails gives the claim back, so a retry makes it.
    async fn move_router(
        &self,
        thread: ThreadId,
        turn: &TurnBrief,
        trouble: Trouble,
        handed: Handed,
    ) -> anyhow::Result<()> {
        let thread_id = thread.to_string();
        let current = self.inner.host.thread_profile(thread).await.unwrap_or(None);
        let pool = match self.live_pool().await {
            Ok(pool) => pool,
            Err(err) => {
                self.release_claim(ROUTER_MOVED_KEY, &turn.id).await?;
                return Err(err);
            }
        };
        if let Some(name) = &current {
            self.cool_down(&pool, name, trouble).await?;
        }
        let now = now();
        let cooling = self.cooldowns().await?;
        let thresholds = self.inner.policy.thresholds;
        let Some(next) =
            accounts::pick(&pool, &cooling, now, current.as_deref(), thresholds).cloned()
        else {
            tracing::warn!(
                "superagent: the Router's account cannot carry its turn ({}) and no account is free",
                trouble.describe()
            );
            return Ok(());
        };
        if let Err(err) = self
            .inner
            .host
            .bind_profile(thread, next.name.clone())
            .await
        {
            self.release_claim(ROUTER_MOVED_KEY, &turn.id).await?;
            return Err(err);
        }
        self.unstuck(&thread_id);
        self.note_move(
            &thread_id,
            current.as_deref(),
            &next.name,
            &format!("Router {}", trouble.describe()),
        )
        .await;
        let words = turn.user.join("\n\n").trim().to_owned();
        if words.is_empty() {
            return Ok(());
        }
        let from = current.unwrap_or_else(|| "原账号".to_owned());
        let text = match handed {
            Handed::AfterFailure => format!(
                "[通知] Router 的账号{}（{from}），已换到 {}。下面是刚才没处理完的话，请接着处理：\n{words}",
                trouble.describe(),
                next.label()
            ),
            Handed::AfterRescue => format!(
                "[通知] Router 的账号{}，刚才那一轮一直在重试（{from}），已停下并换到 {}。下面是刚才没处理完的话，请接着处理：\n{words}",
                trouble.describe(),
                next.label()
            ),
        };
        self.inner
            .host
            .deliver(thread, text, DeliveryMode::Steer)
            .await
    }
}
