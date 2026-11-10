//! The account pool as the drive reads it, the moves it made, and the account check before each
//! step of a driven session.
//!
//! Reading the pool runs the account status helper, which takes seconds: the watch
//! (`router_account.rs`) reads it every couple of minutes, and the checks between turns and before
//! steps use what it read, reading again only when that is older than the policy allows. A move
//! is claimed in the ledger first, so a thread moves at most once per step (a driven session) or
//! per turn away from any one account (the Router), whoever looks.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::time::Instant;

use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;

use super::DriveService;
use super::accounts;
use super::accounts::AccountInfo;
use super::accounts::AccountSource;
use super::accounts::Activity;
use super::accounts::Headroom;
use super::accounts::Plan;
use super::accounts::now;
use crate::core::now_iso;

/// The ledger key of the account moves the engine made, newest last.
const MOVES_KEY: &str = "drive-account-moves";
const MOVES: usize = 30;
/// The ledger key of the drive steps whose account the engine already moved.
pub(super) const STEP_MOVED_KEY: &str = "drive-step-moved";
/// How many claims a ledger list keeps.
const CLAIMS: usize = 50;

/// What the account checks keep in memory between calls.
#[derive(Default)]
pub(super) struct Watch {
    /// The pool as last read.
    pub(super) pool: Vec<AccountInfo>,
    /// When the pool was last read or tried: a failed read counts, so a broken helper is not
    /// asked before every step.
    pub(super) tried: Option<Instant>,
    /// The last read failed and its warning is out.
    pub(super) failing: bool,
    /// The turn each Router runs now, from its start hook.
    pub(super) turns: HashMap<String, String>,
    /// The turn of each Router its account failed (quota used up, model at capacity).
    pub(super) troubled: HashMap<String, String>,
    /// Routers whose queued input waits, until then, for the move at the end of their turn.
    pub(super) held: HashMap<String, Instant>,
    /// Routers to move once their running turn ends, and where to.
    pub(super) pending: HashMap<String, String>,
    /// Threads whose "no account is free" warning is out.
    pub(super) stuck: HashSet<String>,
}

/// An account move the engine made, for status.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccountMove {
    /// RFC 3339, UTC.
    pub at: String,
    pub thread_id: String,
    /// The account it left, when known.
    pub from: Option<String>,
    pub to: String,
    /// Why, in the person's words.
    pub why: String,
}

/// The claim of step `n` of the drive started at `since` on a session.
pub(super) fn step_key(thread_id: &str, since: &str, n: u32) -> String {
    format!("{thread_id}@{since}#{n}")
}

impl DriveService {
    pub(super) fn watch(&self) -> MutexGuard<'_, Watch> {
        self.inner
            .watch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    async fn read_pool(&self, source: &dyn AccountSource) -> anyhow::Result<Vec<AccountInfo>> {
        let read = source.accounts().await;
        let mut watch = self.watch();
        watch.tried = Some(Instant::now());
        match read {
            Ok(pool) => {
                if watch.failing {
                    tracing::info!("superagent drive: the account pool reads again");
                }
                watch.failing = false;
                watch.pool = pool.clone();
                Ok(pool)
            }
            Err(err) => {
                if !watch.failing {
                    tracing::warn!("superagent drive: cannot read the account pool: {err:#}");
                }
                watch.failing = true;
                Err(err)
            }
        }
    }

    /// Reads the pool now, for a move after a failure or one the Router asked for.
    pub(super) async fn live_pool(&self) -> anyhow::Result<Vec<AccountInfo>> {
        let source = self
            .inner
            .accounts
            .clone()
            .ok_or_else(|| anyhow::anyhow!("没有账号状态程序（codex-account-status）"))?;
        let _reading = self.inner.refresh.acquire().await?;
        self.read_pool(source.as_ref()).await
    }

    /// The pool as last read; read again first when that is older than the policy allows. A
    /// failed read keeps the last pool (empty when there was none).
    pub(super) async fn recent_pool(&self) -> Vec<AccountInfo> {
        let Some(source) = self.inner.accounts.clone() else {
            return Vec::new();
        };
        // The semaphore is never closed; without a permit the read just does not wait.
        let _reading = self.inner.refresh.acquire().await.ok();
        let max_age = self.inner.policy.pool_max_age();
        let stale = self
            .watch()
            .tried
            .is_none_or(|tried| tried.elapsed() >= max_age);
        if stale {
            // A failure is logged once; the last pool stands in.
            let _ = self.read_pool(source.as_ref()).await;
        }
        self.watch().pool.clone()
    }

    /// The pool as last read, without reading it: for the quick look while a turn ends.
    pub(super) fn cached_pool(&self) -> Vec<AccountInfo> {
        self.watch().pool.clone()
    }

    /// Records `key` in the ledger list `list`; false when it was there already.
    pub(super) async fn claim(&self, list: &str, key: &str) -> anyhow::Result<bool> {
        self.inner
            .store
            .atomic(async |tx| {
                let mut claimed: Vec<String> = tx.kv_get(list).await?.unwrap_or_default();
                if claimed.iter().any(|claim| claim == key) {
                    return Ok(false);
                }
                claimed.push(key.to_owned());
                if claimed.len() > CLAIMS {
                    claimed.remove(0);
                }
                tx.kv_set(list, &claimed).await?;
                Ok(true)
            })
            .await
    }

    pub(super) async fn claimed(&self, list: &str, key: &str) -> anyhow::Result<bool> {
        let claimed: Vec<String> = self.inner.store.kv_get(list).await?.unwrap_or_default();
        Ok(claimed.iter().any(|claim| claim == key))
    }

    /// Gives a claim back after the move failed, so a retry makes it.
    pub(super) async fn release_claim(&self, list: &str, key: &str) -> anyhow::Result<()> {
        self.inner
            .store
            .atomic(async |tx| {
                let mut claimed: Vec<String> = tx.kv_get(list).await?.unwrap_or_default();
                claimed.retain(|claim| claim != key);
                tx.kv_set(list, &claimed).await
            })
            .await
    }

    /// Keeps a move for status; a failure to keep it is only logged.
    pub(super) async fn note_move(&self, thread_id: &str, from: Option<&str>, to: &str, why: &str) {
        tracing::info!(
            "superagent drive: moved {thread_id} from {} to {to}: {why}",
            from.unwrap_or("-")
        );
        let entry = AccountMove {
            at: now_iso(),
            thread_id: thread_id.to_owned(),
            from: from.map(str::to_owned),
            to: to.to_owned(),
            why: why.to_owned(),
        };
        let kept = self
            .inner
            .store
            .atomic(async |tx| {
                let mut moves: Vec<AccountMove> = tx.kv_get(MOVES_KEY).await?.unwrap_or_default();
                moves.push(entry);
                if moves.len() > MOVES {
                    moves.remove(0);
                }
                tx.kv_set(MOVES_KEY, &moves).await
            })
            .await;
        if let Err(err) = kept {
            tracing::warn!("superagent drive: the move of {thread_id} was not kept: {err:#}");
        }
    }

    /// The account moves the engine made, oldest first (at most the last 30).
    pub async fn account_moves(&self) -> anyhow::Result<Vec<AccountMove>> {
        Ok(self
            .inner
            .store
            .kv_get(MOVES_KEY)
            .await?
            .unwrap_or_default())
    }

    /// A thread that should leave its account finds none free: warned once, until that changes.
    pub(super) fn stuck(&self, thread_id: &str, current: &str, headroom: Headroom) {
        if self.watch().stuck.insert(thread_id.to_owned()) {
            tracing::warn!(
                "superagent drive: {thread_id} should leave {current} ({headroom:?}) but no account is free"
            );
        }
    }

    pub(super) fn unstuck(&self, thread_id: &str) {
        self.watch().stuck.remove(thread_id);
    }

    /// The pool's name of the account the Router named (in any case). Without the account status
    /// helper, or when the pool cannot be read, the name stands as given.
    pub(super) async fn resolve_account(&self, wanted: &str) -> anyhow::Result<String> {
        let wanted = wanted.trim();
        anyhow::ensure!(!wanted.is_empty(), "账号名是空的");
        let pool = self.recent_pool().await;
        if pool.is_empty() {
            return Ok(wanted.to_owned());
        }
        pool.iter()
            .find(|account| account.name.eq_ignore_ascii_case(wanted))
            .map(|account| account.name.clone())
            .ok_or_else(|| anyhow::anyhow!("账号池里没有 {wanted}"))
    }

    /// Opens a new session in `folder` (created when missing) under the account the Router named.
    pub async fn open_session_on(&self, folder: &Path, account: &str) -> anyhow::Result<String> {
        let account = self.resolve_account(account).await?;
        self.open_session_under(folder, Some(account)).await
    }

    /// The Router starts a drive on a named account: the session moves there before step 1, and
    /// the check before that step leaves it there. Answers the account's name.
    pub(super) async fn bind_start_account(
        &self,
        thread_id: &str,
        thread: ThreadId,
        wanted: &str,
        since: &str,
    ) -> anyhow::Result<String> {
        let name = self.resolve_account(wanted).await?;
        let from = self.inner.host.thread_profile(thread).await.unwrap_or(None);
        self.inner
            .host
            .bind_profile(thread, name.clone())
            .await
            .map_err(|err| anyhow::anyhow!("会话换到 {name} 失败：{err:#}"))?;
        self.claim(STEP_MOVED_KEY, &step_key(thread_id, since, 1))
            .await?;
        if from.as_deref() != Some(name.as_str()) {
            self.note_move(thread_id, from.as_deref(), &name, "推进开始时指定的账号")
                .await;
        }
        Ok(name)
    }

    /// Before step `n` goes to a driven session: when its account runs low (below the soft
    /// threshold, cooling down or not usable), the session moves to the best account first, and
    /// the drive records the account as `switch_account` does. A session whose turn runs (the
    /// person typed into it) is left alone: a step never moves midway. A failed check is logged
    /// and the step goes out all the same.
    pub(super) async fn check_step_account(&self, thread_id: &str, thread: ThreadId, n: u32) {
        if self.inner.accounts.is_none() {
            return;
        }
        let checked = async {
            let Some(record) = self.record(thread_id).await? else {
                return anyhow::Ok(());
            };
            let key = step_key(thread_id, &record.since, n);
            if self.claimed(STEP_MOVED_KEY, &key).await? {
                return Ok(());
            }
            // A cold session reports its account once loaded; the step loads it anyway.
            self.inner.host.resume_thread(thread, None).await?;
            let Some(current) = self.inner.host.thread_profile(thread).await? else {
                return Ok(());
            };
            let running = self
                .inner
                .host
                .session_turns(thread, 1)
                .await?
                .first()
                .is_some_and(|turn| turn.status == "inProgress");
            if running {
                return Ok(());
            }
            let pool = self.recent_pool().await;
            let cooling = self.cooldowns().await?;
            let now = now();
            let thresholds = self.inner.policy.thresholds;
            match accounts::plan(&pool, &cooling, now, &current, thresholds, Activity::Idle) {
                Plan::Move { to, .. } => {
                    if !self.claim(STEP_MOVED_KEY, &key).await? {
                        return Ok(());
                    }
                    if let Err(err) = self.inner.host.bind_profile(thread, to.name.clone()).await {
                        self.release_claim(STEP_MOVED_KEY, &key).await?;
                        return Err(err);
                    }
                    self.unstuck(thread_id);
                    let name = to.name.clone();
                    self.change(thread_id, |record| record.account = Some(name))
                        .await?;
                    let why = accounts::shortfall(&pool, &cooling, now, &current);
                    self.note_move(
                        thread_id,
                        Some(&current),
                        &to.name,
                        &format!("第 {n} 步前原账号{why}"),
                    )
                    .await;
                }
                Plan::NoneFree(headroom) => self.stuck(thread_id, &current, headroom),
                Plan::Stay | Plan::AtTurnEnd { .. } => self.unstuck(thread_id),
            }
            Ok(())
        }
        .await;
        if let Err(err) = checked {
            tracing::warn!(
                "superagent drive: the account check before step {n} of {thread_id} failed: {err:#}"
            );
        }
    }
}
