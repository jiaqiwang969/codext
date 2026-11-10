//! The person's reset cards, used by the engine itself when no account can take a move; off
//! unless `CODEXT_SUPERAGENT_RESET_CARDS_PER_DAY` allows some. A card refills one used-up account
//! to full and starts its weekly window again from the moment it is used. It adds nothing to what
//! is left, and what it gives is lost at that account's next natural reset. So the engine cards
//! only an account that is used up and whose natural reset is at least `card_reset_away` off, the
//! one that resets last first, with that account's card that expires first. Each use is told to
//! the Router and kept (the last `KEPT`, in `superagent/status`).

use std::time::Instant;

use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

use super::DriveService;
use super::accounts::AccountInfo;
use crate::core::now_iso;
use crate::host::ResetCard;
use crate::host::ResetOutcome;

/// The ledger key of the cards the engine used.
pub const CARDS_KEY: &str = "drive-reset-cards";
const KEPT: usize = 30;
/// A card refills the account for this long, unless used up first.
const WEEK_SECS: i64 = 7 * 24 * 3600;

/// One card the engine used.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CardUse {
    pub at: String,
    pub account: String,
    pub card: String,
    /// What the backend answered: `reset`, `nothing_to_reset`, `no_card`, `already_used`.
    pub outcome: String,
    /// The account's natural reset before the card, Unix seconds.
    pub natural_reset: Option<i64>,
}

impl ResetOutcome {
    fn name(self) -> &'static str {
        match self {
            Self::Reset => "reset",
            Self::NothingToReset => "nothing_to_reset",
            Self::NoCard => "no_card",
            Self::AlreadyUsed => "already_used",
        }
    }
}

/// The accounts a card may refill, the one whose natural reset is furthest off first: used up
/// (below `hard` percent), not free, other than `except`, and resetting no sooner than
/// `min_away` seconds from `now`.
pub fn card_candidates<'a>(
    pool: &'a [AccountInfo],
    now: i64,
    hard: f64,
    min_away: i64,
    except: Option<&str>,
) -> Vec<&'a AccountInfo> {
    let mut candidates: Vec<&AccountInfo> = pool
        .iter()
        .filter(|account| account.plan != "free" && account.remaining < hard)
        .filter(|account| except != Some(account.name.as_str()))
        .filter(|account| account.resets_at.is_some_and(|at| at - now >= min_away))
        .collect();
    candidates.sort_by_key(|account| std::cmp::Reverse(account.resets_at));
    candidates
}

/// The available card that expires first.
pub fn first_to_expire(cards: &[ResetCard]) -> Option<&ResetCard> {
    cards
        .iter()
        .filter(|card| card.available)
        .min_by_key(|card| card.expires_at.unwrap_or(i64::MAX))
}

impl DriveService {
    /// The cards the engine used, oldest first (at most the last 30).
    pub async fn card_uses(&self) -> anyhow::Result<Vec<CardUse>> {
        Ok(self
            .inner
            .store
            .kv_get(CARDS_KEY)
            .await?
            .unwrap_or_default())
    }

    /// No account in `pool` can take a move off `current`: refills one with a reset card, as the
    /// policy allows, and answers it as it reads after the card. `None` when the policy is off,
    /// today's cards are used, or no account qualifies or has a card.
    pub(super) async fn refill_with_card(
        &self,
        pool: &[AccountInfo],
        now: i64,
        current: Option<&str>,
    ) -> Option<AccountInfo> {
        let policy = &self.inner.policy;
        if policy.cards_per_day == 0 {
            return None;
        }
        let uses = match self.card_uses().await {
            Ok(uses) => uses,
            Err(err) => {
                tracing::warn!("superagent drive: the reset cards used are unreadable: {err:#}");
                return None;
            }
        };
        let day_ago = Utc::now() - chrono::Duration::hours(24);
        let today = uses
            .iter()
            .filter(|used| used.outcome == ResetOutcome::Reset.name())
            .filter(|used| {
                chrono::DateTime::parse_from_rfc3339(&used.at).is_ok_and(|at| at > day_ago)
            })
            .count();
        if today >= policy.cards_per_day {
            tracing::info!(
                "superagent drive: no account can take a move, and the {} reset cards of the last day are used",
                policy.cards_per_day
            );
            return None;
        }
        let min_away = i64::try_from(policy.card_reset_away.as_secs()).unwrap_or(i64::MAX);
        for account in card_candidates(pool, now, policy.thresholds.hard, min_away, current) {
            let cards = match self.inner.host.reset_cards(account.name.clone()).await {
                Ok(cards) => cards,
                Err(err) => {
                    tracing::warn!(
                        "superagent drive: the reset cards of {} are unreadable: {err:#}",
                        account.name
                    );
                    continue;
                }
            };
            let Some(card) = first_to_expire(&cards).cloned() else {
                continue;
            };
            // One key per use the engine decides on: a repeated request uses the card once.
            let key = format!("superagent-{}-{}", card.id, Utc::now().timestamp_millis());
            let outcome = match self
                .inner
                .host
                .use_reset_card(account.name.clone(), card.id.clone(), key)
                .await
            {
                Ok(outcome) => outcome,
                Err(err) => {
                    tracing::warn!(
                        "superagent drive: the reset card {} of {} was not used: {err:#}",
                        card.id,
                        account.name
                    );
                    continue;
                }
            };
            let used = CardUse {
                at: now_iso(),
                account: account.name.clone(),
                card: card.id.clone(),
                outcome: outcome.name().to_owned(),
                natural_reset: account.resets_at,
            };
            let kept = self
                .inner
                .store
                .atomic(async |tx| {
                    let mut uses: Vec<CardUse> = tx.kv_get(CARDS_KEY).await?.unwrap_or_default();
                    uses.push(used);
                    let excess = uses.len().saturating_sub(KEPT);
                    uses.drain(..excess);
                    tx.kv_set(CARDS_KEY, &uses).await
                })
                .await;
            if let Err(err) = kept {
                tracing::warn!("superagent drive: a reset card use was not kept: {err:#}");
            }
            match outcome {
                ResetOutcome::Reset => {
                    let refilled = AccountInfo {
                        usable: true,
                        remaining: 100.0,
                        resets_at: Some(now + WEEK_SECS),
                        ..account.clone()
                    };
                    // Checks that come next go by this read, not the pool's older one.
                    self.watch()
                        .seen
                        .insert(account.name.clone(), (refilled.clone(), Instant::now()));
                    self.end_refilled_cooldowns(std::slice::from_ref(&refilled))
                        .await;
                    let summary = format!(
                        "[通知] 账号都快用完了，引擎用了一张重置卡给 {}（它原本 {} 才自然重置），这一天第 {} 张，上限 {} 张。",
                        account.name,
                        account
                            .resets_at
                            .and_then(|at| chrono::DateTime::from_timestamp(at, 0))
                            .map(|at| at
                                .with_timezone(&chrono::Local)
                                .format("%m-%d %H:%M")
                                .to_string())
                            .unwrap_or_else(|| "不知何时".to_owned()),
                        today + 1,
                        policy.cards_per_day
                    );
                    if let Err(err) = self
                        .tell_router(&format!("card:{}", card.id), summary)
                        .await
                    {
                        tracing::warn!("superagent drive: a reset card use was not told: {err:#}");
                    }
                    return Some(refilled);
                }
                // The account is not used up after all: the next read of the pool shows it.
                ResetOutcome::NothingToReset => return None,
                ResetOutcome::NoCard | ResetOutcome::AlreadyUsed => continue,
            }
        }
        None
    }
}
