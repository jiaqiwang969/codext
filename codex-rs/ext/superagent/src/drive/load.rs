//! How many sessions each account carries, so the engine spreads the sessions it moves. The
//! person's rule is three or four sessions per account: a move the engine makes by itself passes
//! by accounts that already carry `max_per_account` sessions at work (driven sessions working on
//! a step or waiting for their next one, and the Routers). When every other account is that
//! full, the move goes as it would without the rule; a crowded account beats none.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;

use codex_protocol::ThreadId;

use super::DriveRecord;
use super::DriveService;
use super::DriveState;
use super::accounts;
use super::accounts::AccountInfo;
use super::accounts::Thresholds;
use super::cooldown::Cooldown;

impl DriveService {
    /// The account each session at work runs on now, by thread: the driven sessions working on a
    /// step or waiting for their next one, and the Routers.
    pub(super) async fn accounts_at_work(
        &self,
        records: &[DriveRecord],
    ) -> HashMap<String, String> {
        let mut threads: Vec<String> = records
            .iter()
            .filter(|record| {
                matches!(
                    record.state,
                    DriveState::Driving | DriveState::Reported { .. }
                )
            })
            .map(|record| record.thread_id.clone())
            .collect();
        threads.extend((self.inner.routers)());
        let mut on = HashMap::new();
        for thread in threads {
            let Ok(id) = ThreadId::from_string(&thread) else {
                continue;
            };
            if let Ok(Some(account)) = self.inner.host.thread_profile(id).await {
                on.insert(thread, account);
            }
        }
        on
    }

    /// The sessions at work, by the account each runs on now.
    pub(super) async fn account_load(&self) -> anyhow::Result<HashMap<String, usize>> {
        let records = self.records().await?;
        let mut load = HashMap::new();
        for account in self.accounts_at_work(&records).await.into_values() {
            *load.entry(account).or_insert(0) += 1;
        }
        Ok(load)
    }

    /// The accounts a session should not be moved to while another has room: those already
    /// carrying the most sessions the policy allows. Empty when the rule is off.
    pub(super) async fn crowded(&self) -> anyhow::Result<HashSet<String>> {
        let cap = self.inner.policy.max_per_account;
        if cap == 0 {
            return Ok(HashSet::new());
        }
        Ok(self
            .account_load()
            .await?
            .into_iter()
            .filter(|(_, sessions)| *sessions >= cap)
            .map(|(account, _)| account)
            .collect())
    }

    /// `pool` without the crowded accounts; `current` stays, as the account being left.
    pub(super) async fn roomy(
        &self,
        pool: &[AccountInfo],
        current: Option<&str>,
    ) -> anyhow::Result<Vec<AccountInfo>> {
        let crowded = self.crowded().await?;
        Ok(pool
            .iter()
            .filter(|account| {
                Some(account.name.as_str()) == current || !crowded.contains(&account.name)
            })
            .cloned()
            .collect())
    }

    /// The account the engine moves a session on `current` to: as `accounts::pick` takes it among
    /// the accounts with room, else among all.
    pub(super) async fn pick_spread(
        &self,
        pool: &[AccountInfo],
        cooling: &BTreeMap<String, Cooldown>,
        now: i64,
        current: Option<&str>,
        thresholds: Thresholds,
    ) -> anyhow::Result<Option<AccountInfo>> {
        let roomy = self.roomy(pool, current).await?;
        Ok(accounts::pick(&roomy, cooling, now, current, thresholds)
            .or_else(|| accounts::pick(pool, cooling, now, current, thresholds))
            .cloned())
    }
}
