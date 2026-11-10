//! The accounts cooling down (`accounts.rs`). An account cools down after a turn it could not
//! carry: until its quota resets when the quota ran out, a while when the model was at capacity on
//! it. The person often refills a spent account with a reset card, so a fresh read that shows the
//! quota back ends a usage-limit cooldown there and then, and the check that made the read can take
//! the account at once. A capacity cooldown is short and runs its time.
//!
//! The ledger keeps one row per account under `COOLDOWN_KEY`. Rows of earlier versions have only
//! `until` and `reason`; their reason tells their kind.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

use super::DriveService;
use super::accounts::AccountInfo;
use super::accounts::QUOTA_COOLDOWN;
use super::accounts::Trouble;
use super::accounts::now;
use crate::core::now_iso;

/// The ledger key of the accounts cooling down.
pub const COOLDOWN_KEY: &str = "drive-account-cooldowns";
/// The ledger key of the cooldowns a fresh read ended early, newest last.
const ENDED_KEY: &str = "drive-account-cooldowns-ended";
const ENDED: usize = 30;

/// An account cooling down until `until` (Unix seconds).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Cooldown {
    pub until: i64,
    /// Why, in the person's words.
    pub reason: String,
    /// What the account could not carry; rows of earlier versions have none, their reason says it.
    #[serde(default)]
    pub kind: Option<Trouble>,
    /// The quota left (percent) the account showed when it cooled down; `None` when it could not
    /// be read then, and in rows of earlier versions.
    #[serde(default)]
    pub remaining: Option<f64>,
}

impl Cooldown {
    /// What the account could not carry: the row's kind, else what its reason names. `None` when
    /// neither tells; such a cooldown just runs its time.
    pub fn trouble(&self) -> Option<Trouble> {
        self.kind.or_else(|| {
            if self.reason.contains("额度") {
                Some(Trouble::UsageLimit)
            } else if self.reason.contains("满载") || self.reason.contains("capacity") {
                Some(Trouble::Capacity)
            } else {
                None
            }
        })
    }

    /// Whether `read`, a fresh read of the account, ends this cooldown: a usage-limit one, once the
    /// account is usable with at least `soft` percent left, more than it showed when it cooled
    /// down. An account whose read showed that much even then ran out of something its reads do
    /// not show, so its cooldown runs its time.
    pub fn ended_by(&self, read: &AccountInfo, soft: f64) -> bool {
        self.trouble() == Some(Trouble::UsageLimit)
            && read.usable
            && read.remaining >= soft
            && self.remaining.is_none_or(|seen| read.remaining > seen)
    }
}

/// A usage-limit cooldown a fresh read ended early, for status.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EndedCooldown {
    /// RFC 3339, UTC.
    pub at: String,
    pub account: String,
    /// What happened, in the person's words.
    pub why: String,
}

impl DriveService {
    /// The accounts cooling down now.
    pub async fn cooldowns(&self) -> anyhow::Result<BTreeMap<String, Cooldown>> {
        let all: BTreeMap<String, Cooldown> = self
            .inner
            .store
            .kv_get(COOLDOWN_KEY)
            .await?
            .unwrap_or_default();
        let now = now();
        Ok(all
            .into_iter()
            .filter(|(_, cooldown)| cooldown.until > now)
            .collect())
    }

    /// `account` cools down after `trouble`, as `pool`, read just before, shows it: until its
    /// quota resets (an hour when that is unknown), or a while for a model at capacity.
    pub(super) async fn cool_down(
        &self,
        pool: &[AccountInfo],
        account: &str,
        trouble: Trouble,
    ) -> anyhow::Result<()> {
        let now = now();
        let read = pool.iter().find(|info| info.name == account);
        let until = match trouble {
            Trouble::UsageLimit => read
                .and_then(|info| info.resets_at)
                .filter(|at| *at > now)
                .unwrap_or(now + QUOTA_COOLDOWN.as_secs() as i64),
            Trouble::Capacity => now + self.inner.capacity_cooldown.as_secs() as i64,
        };
        let cooldown = Cooldown {
            until,
            reason: trouble.describe().to_owned(),
            kind: Some(trouble),
            remaining: read.filter(|info| info.usable).map(|info| info.remaining),
        };
        let account = account.to_owned();
        self.inner
            .store
            .atomic(async |tx| {
                let mut all: BTreeMap<String, Cooldown> =
                    tx.kv_get(COOLDOWN_KEY).await?.unwrap_or_default();
                all.retain(|_, cooldown| cooldown.until > now);
                all.insert(account, cooldown);
                tx.kv_set(COOLDOWN_KEY, &all).await
            })
            .await
    }

    /// After a fresh read of `accounts`: the usage-limit cooldown of each account it shows with
    /// its quota back (`Cooldown::ended_by`) ends, in one transaction, so the check that made the
    /// read finds the account free. Each cooldown ended is logged once and kept for status; a
    /// failure to end them is only logged.
    pub(super) async fn end_refilled_cooldowns(&self, accounts: &[AccountInfo]) {
        let soft = self.inner.policy.thresholds.soft;
        let now = now();
        let ended = self
            .inner
            .store
            .atomic(async |tx| {
                let mut all: BTreeMap<String, Cooldown> =
                    tx.kv_get(COOLDOWN_KEY).await?.unwrap_or_default();
                let ended: Vec<(String, f64)> = accounts
                    .iter()
                    .filter(|read| {
                        all.get(&read.name).is_some_and(|cooldown| {
                            cooldown.until > now && cooldown.ended_by(read, soft)
                        })
                    })
                    .map(|read| (read.name.clone(), read.remaining))
                    .collect();
                if ended.is_empty() {
                    return Ok(ended);
                }
                all.retain(|name, cooldown| {
                    cooldown.until > now && !ended.iter().any(|(account, _)| account == name)
                });
                tx.kv_set(COOLDOWN_KEY, &all).await?;
                let mut kept: Vec<EndedCooldown> = tx.kv_get(ENDED_KEY).await?.unwrap_or_default();
                kept.extend(ended.iter().map(|(account, remaining)| EndedCooldown {
                    at: now_iso(),
                    account: account.clone(),
                    why: format!("额度恢复（剩 {remaining:.0}%），提前结束冷却"),
                }));
                let excess = kept.len().saturating_sub(ENDED);
                kept.drain(..excess);
                tx.kv_set(ENDED_KEY, &kept).await?;
                Ok(ended)
            })
            .await;
        match ended {
            Ok(ended) => {
                for (account, remaining) in ended {
                    tracing::info!(
                        "superagent drive: the account {account} has quota again ({remaining:.0}% left); its usage-limit cooldown ends"
                    );
                }
            }
            Err(err) => tracing::warn!(
                "superagent drive: the cooldowns of accounts with quota again did not end: {err:#}"
            ),
        }
    }

    /// The usage-limit cooldowns fresh reads ended early, oldest first (at most the last 30).
    pub async fn ended_cooldowns(&self) -> anyhow::Result<Vec<EndedCooldown>> {
        Ok(self
            .inner
            .store
            .kv_get(ENDED_KEY)
            .await?
            .unwrap_or_default())
    }
}
