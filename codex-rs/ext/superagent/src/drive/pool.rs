//! What the drive knows of the accounts, and the moves it made.
//!
//! A check reads only the account a thread runs on, on its own and through the daemon (cheap);
//! only when that account should be left does it read the whole pool, which runs the account
//! status helper over every account and takes seconds. Both reads are kept for the policy's
//! maximum age, so the checks between turns and before steps mostly read nothing. A move is
//! claimed in the ledger first, so a thread moves at most once per step (a driven session) or per
//! turn away from any one account (the Router), whoever looks.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;

use super::DriveService;
use super::accounts;
use super::accounts::AccountInfo;
use super::accounts::AccountSource;
use super::accounts::Activity;
use super::accounts::Cooldown;
use super::accounts::Headroom;
use super::accounts::Plan;
use super::accounts::now;
use crate::core::now_iso;

/// The ledger key of the account moves the engine made, newest last.
const MOVES_KEY: &str = "drive-account-moves";
const MOVES: usize = 30;
/// How many claims a ledger list keeps.
const CLAIMS: usize = 50;

/// What the account checks keep in memory between calls.
#[derive(Default)]
pub(super) struct Watch {
    /// The pool as last read in full.
    pub(super) pool: Vec<AccountInfo>,
    /// When the pool was last read in full.
    pub(super) pool_at: Option<Instant>,
    /// When the pool was last read or tried: a failed read counts, so a broken helper is not
    /// asked before every check.
    pub(super) tried: Option<Instant>,
    /// The last full read failed and its warning is out.
    pub(super) failing: bool,
    /// Each account as last read on its own, and when.
    pub(super) seen: HashMap<String, (AccountInfo, Instant)>,
    /// Accounts whose own read fails, their warning out.
    pub(super) unread: HashSet<String>,
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

/// How fresh the quota of the account a thread runs on must be for a check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Look {
    /// Read it now: the watch.
    Now,
    /// As read within the policy's maximum age: a check between turns or before a step.
    Recent,
}

/// The account a thread runs on, as read on its own, against its cooldown.
pub(super) struct Standing {
    current: String,
    /// The account's own read; `None` when it failed.
    own: Option<AccountInfo>,
    cooling: BTreeMap<String, Cooldown>,
    headroom: Option<Headroom>,
}

impl Standing {
    /// Enough quota left, not cooling down and usable: nothing more to read.
    pub(super) fn healthy(&self) -> bool {
        self.headroom == Some(Headroom::Enough)
    }
}

/// Whether and where a thread moves, and why, in the person's words.
pub(super) struct Verdict {
    pub(super) plan: Plan,
    pub(super) why: String,
}

impl DriveService {
    pub(super) fn watch(&self) -> MutexGuard<'_, Watch> {
        self.inner
            .watch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn max_age(&self) -> Duration {
        self.inner.policy.pool_max_age()
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
                watch.pool_at = watch.tried;
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

    /// Reads the whole pool now, for a move after a failure or one the Router asked for.
    pub(super) async fn live_pool(&self) -> anyhow::Result<Vec<AccountInfo>> {
        let source = self
            .inner
            .accounts
            .clone()
            .ok_or_else(|| anyhow::anyhow!("没有账号状态程序（codex-account-status）"))?;
        let _reading = self.inner.refresh.acquire().await?;
        self.read_pool(source.as_ref()).await
    }

    /// The whole pool as last read; read again first when that is older than the policy allows.
    /// A failed read keeps the last pool (empty when there was none).
    pub(super) async fn recent_pool(&self) -> Vec<AccountInfo> {
        let Some(source) = self.inner.accounts.clone() else {
            return Vec::new();
        };
        // The semaphore is never closed; without a permit the read just does not wait.
        let _reading = self.inner.refresh.acquire().await.ok();
        let max_age = self.max_age();
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

    /// The newest that is known of an account, read on its own or with the pool, when it is
    /// not older than `max_age`.
    fn known_account(&self, name: &str, max_age: Option<Duration>) -> Option<AccountInfo> {
        let watch = self.watch();
        let own = watch.seen.get(name).cloned();
        let pooled = watch.pool_at.and_then(|at| {
            watch
                .pool
                .iter()
                .find(|account| account.name == name)
                .map(|account| (account.clone(), at))
        });
        let newest = match (own, pooled) {
            (Some(own), Some(pooled)) => Some(if own.1 >= pooled.1 { own } else { pooled }),
            (own, pooled) => own.or(pooled),
        };
        newest
            .filter(|(_, at)| max_age.is_none_or(|max_age| at.elapsed() < max_age))
            .map(|(account, _)| account)
    }

    /// What is known of an account without reading anything: for the quick look while a turn
    /// ends.
    pub(super) fn cached_account(&self, name: &str) -> Option<AccountInfo> {
        self.known_account(name, /*max_age*/ None)
    }

    /// One account's quota: as read within the policy's age for `Look::Recent`, else read on its
    /// own now. A failed read is warned once and answers `None`.
    async fn account_info(&self, name: &str, look: Look) -> Option<AccountInfo> {
        let source = self.inner.accounts.clone()?;
        if look == Look::Recent
            && let Some(account) = self.known_account(name, Some(self.max_age()))
        {
            return Some(account);
        }
        let read = source.account(name).await;
        let mut watch = self.watch();
        match read {
            Ok(account) => {
                if watch.unread.remove(name) {
                    tracing::info!("superagent drive: the account {name} reads again");
                }
                watch
                    .seen
                    .insert(name.to_owned(), (account.clone(), Instant::now()));
                Some(account)
            }
            Err(err) => {
                if watch.unread.insert(name.to_owned()) {
                    tracing::warn!("superagent drive: cannot read the account {name}: {err:#}");
                }
                None
            }
        }
    }

    /// How the account `current` stands: read on its own (cheap), against its cooldown.
    pub(super) async fn standing(&self, current: &str, look: Look) -> anyhow::Result<Standing> {
        let cooling = self.cooldowns().await?;
        let own = self.account_info(current, look).await;
        let thresholds = self.inner.policy.thresholds;
        let headroom = own.as_ref().and_then(|account| {
            accounts::headroom(
                std::slice::from_ref(account),
                &cooling,
                now(),
                current,
                thresholds,
            )
        });
        Ok(Standing {
            current: current.to_owned(),
            own,
            cooling,
            headroom,
        })
    }

    /// Where a thread whose account is not healthy moves: the whole pool decides (read again
    /// when older than the policy allows), each account as most recently read, so an account
    /// just found low on its own is not taken for the healthy one the older pool shows. When the
    /// thread's own read failed, the pool's entry for its account decides whether it moves.
    pub(super) async fn plan_move(&self, standing: Standing, activity: Activity) -> Verdict {
        let Standing {
            current,
            own,
            cooling,
            ..
        } = standing;
        let mut pool = self.recent_pool().await;
        let newer: Vec<AccountInfo> = {
            let watch = self.watch();
            watch
                .seen
                .values()
                .filter(|(_, at)| watch.pool_at.is_none_or(|pool_at| *at > pool_at))
                .map(|(account, _)| account.clone())
                .chain(own)
                .collect()
        };
        for read in newer {
            match pool.iter_mut().find(|account| account.name == read.name) {
                // A read on its own may not say the plan; the pool's does.
                Some(pooled) => {
                    let plan = if read.plan.is_empty() {
                        std::mem::take(&mut pooled.plan)
                    } else {
                        read.plan.clone()
                    };
                    *pooled = AccountInfo { plan, ..read };
                }
                None => pool.push(read),
            }
        }
        let now = now();
        let thresholds = self.inner.policy.thresholds;
        Verdict {
            plan: accounts::plan(&pool, &cooling, now, &current, thresholds, activity),
            why: accounts::shortfall(&pool, &cooling, now, &current),
        }
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
}
