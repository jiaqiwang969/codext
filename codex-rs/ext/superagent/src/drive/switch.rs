//! Moving a driven session to another account (`accounts.rs`): by itself when a step's account
//! could not carry it (out of quota, model at capacity), or when the Router asks.

use std::collections::BTreeMap;

use codex_protocol::ThreadId;
use serde_json::json;

use super::DriveRecord;
use super::DriveService;
use super::DriveState;
use super::KEY_PREFIX;
use super::NOTICE;
use super::accounts;
use super::accounts::Cooldown;
use super::accounts::Trouble;
use crate::core::now_iso;

/// How the pause while an account is being switched ends; only such a pause is resumed by it.
pub const SWITCHING: &str = "正在换号";

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn local_time(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0)
        .map(|at| {
            at.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| at.to_string())
}

/// What step `n` told the session: its message, then what was said into its turn.
fn step_text(record: &DriveRecord) -> Option<String> {
    let n = record.steps;
    let mut parts = record.history.iter().filter(|step| step.n == n);
    let first = parts.find(|step| !step.steered)?;
    let mut text = first.text.clone();
    for steered in record
        .history
        .iter()
        .filter(|step| step.n == n && step.steered)
    {
        text.push_str("\n\n（中途补充）");
        text.push_str(&steered.text);
    }
    Some(text)
}

impl DriveService {
    /// The accounts cooling down now.
    pub async fn cooldowns(&self) -> anyhow::Result<BTreeMap<String, Cooldown>> {
        let all: BTreeMap<String, Cooldown> = self
            .inner
            .store
            .kv_get(accounts::COOLDOWN_KEY)
            .await?
            .unwrap_or_default();
        let now = now();
        Ok(all
            .into_iter()
            .filter(|(_, cooldown)| cooldown.until > now)
            .collect())
    }

    async fn cool_down(&self, account: &str, until: i64, reason: &str) -> anyhow::Result<()> {
        let account = account.to_owned();
        let reason = reason.to_owned();
        let now = now();
        self.inner
            .store
            .atomic(async |tx| {
                let mut all: BTreeMap<String, Cooldown> = tx
                    .kv_get(accounts::COOLDOWN_KEY)
                    .await?
                    .unwrap_or_default();
                all.retain(|_, cooldown| cooldown.until > now);
                all.insert(account, Cooldown { until, reason });
                tx.kv_set(accounts::COOLDOWN_KEY, &all).await
            })
            .await
    }

    /// A notice to the Router in its own words.
    async fn tell(&self, record: &DriveRecord, summary: String) -> anyhow::Result<()> {
        let thread_id = &record.thread_id;
        self.inner
            .effects
            .enqueue_now(
                NOTICE,
                &format!("{KEY_PREFIX}{thread_id}"),
                &json!({ "value": { "threadId": thread_id, "account": record.account }, "summary": summary }),
            )
            .await
    }

    /// Gives up switching: the drive stays paused with `reason`, and the Router is told.
    async fn stop_switching(&self, record: &DriveRecord, reason: String) -> anyhow::Result<()> {
        let state = DriveState::Paused { reason };
        let told = state.clone();
        let changed = self
            .change(&record.thread_id, |record| {
                let ours = matches!(&record.state, DriveState::Paused { reason } if reason.ends_with(SWITCHING));
                if ours {
                    record.state = state;
                }
                ours.then(|| record.clone())
            })
            .await?;
        match changed {
            Some(Some(record)) => self.notify(&record, &told).await,
            Some(None) | None => Ok(()),
        }
    }

    /// After step `n` failed on its account: that account cools down (until its quota resets,
    /// or a while for a model at capacity), the session moves to the next account by reset
    /// time, and step `n` goes again there. No account left pauses the drive.
    pub(super) async fn retry_elsewhere(
        &self,
        record: DriveRecord,
        thread: ThreadId,
        trouble: Trouble,
    ) -> anyhow::Result<()> {
        let n = record.steps;
        let Some(source) = self.inner.accounts.clone() else {
            return self
                .stop_switching(&record, format!("第 {n} 步{}", trouble.describe()))
                .await;
        };
        let now = now();
        let current = self.inner.host.thread_profile(thread).await.unwrap_or(None);
        let pool = match source.accounts().await {
            Ok(pool) => pool,
            Err(err) => {
                let reason = format!("第 {n} 步{}，但读不到账号状态：{err:#}", trouble.describe());
                return self.stop_switching(&record, reason).await;
            }
        };
        if let Some(name) = &current {
            let until = match trouble {
                Trouble::UsageLimit => pool
                    .iter()
                    .find(|account| &account.name == name)
                    .and_then(|account| account.resets_at)
                    .filter(|at| *at > now)
                    .unwrap_or(now + accounts::QUOTA_COOLDOWN.as_secs() as i64),
                Trouble::Capacity => now + self.inner.capacity_cooldown.as_secs() as i64,
            };
            self.cool_down(name, until, trouble.describe()).await?;
        }
        let cooling = self.cooldowns().await?;
        let Some(next) = accounts::choose(&pool, &cooling, now, current.as_deref()).cloned() else {
            let soonest = cooling
                .values()
                .map(|cooldown| cooldown.until)
                .min()
                .map(|at| format!("最早 {} 有账号冷却结束", local_time(at)))
                .unwrap_or_else(|| "所有账号的额度都见底了".to_owned());
            let reason = format!("第 {n} 步{}；没有可换的账号，{soonest}", trouble.describe());
            return self.stop_switching(&record, reason).await;
        };
        if let Err(err) = self.inner.host.bind_profile(thread, next.name.clone()).await {
            let reason = format!("第 {n} 步{}；换到 {} 失败：{err:#}", trouble.describe(), next.name);
            return self.stop_switching(&record, reason).await;
        }
        let Some(text) = step_text(&record) else {
            return self
                .stop_switching(&record, format!("第 {n} 步换号后找不到它的原话"))
                .await;
        };
        let account = next.name.clone();
        let changed = self
            .change(&record.thread_id, |record| {
                let ours = matches!(&record.state, DriveState::Paused { reason } if reason.ends_with(SWITCHING))
                    && record.steps == n;
                if ours {
                    record.state = DriveState::Driving;
                    record.inflight = Some(n);
                    record.account = Some(account);
                    if let Some(step) = record
                        .history
                        .iter_mut()
                        .rev()
                        .find(|step| step.n == n && !step.steered)
                    {
                        step.at = now_iso();
                    }
                }
                ours.then(|| record.clone())
            })
            .await?;
        let Some(Some(moved)) = changed else {
            return Ok(());
        };
        let from = current.unwrap_or_else(|| "原账号".to_owned());
        self.tell(
            &moved,
            format!(
                "推进 · {} · 第 {n} 步{}（{from}），已换到 {} 重新开始这一步\n会话 {}\n接下来：不用回它；告诉用户换了号即可。",
                moved.title,
                trouble.describe(),
                next.label(),
                moved.thread_id
            ),
        )
        .await?;
        self.hand_over(&moved.thread_id, thread, &moved.title, n, text)
            .await
    }

    /// The Router moves a session to another account: the one named, or the next by reset time.
    /// Not while a turn runs (interrupt it first). Answers the session's drive record (`None`
    /// for a session that is not driven) and the account it now runs under.
    pub async fn switch_account(
        &self,
        thread_id: &str,
        wanted: Option<String>,
    ) -> anyhow::Result<(Option<DriveRecord>, String)> {
        let thread = ThreadId::from_string(thread_id)
            .map_err(|err| anyhow::anyhow!("会话 id {thread_id} 无效：{err}"))?;
        let running = self
            .inner
            .host
            .session_turns(thread, 1)
            .await?
            .first()
            .is_some_and(|turn| turn.status == "inProgress");
        anyhow::ensure!(!running, "这个会话正在做一轮：先 interrupt，再换号");
        let source = self
            .inner
            .accounts
            .clone()
            .ok_or_else(|| anyhow::anyhow!("没有账号状态程序（codex-account-status），不能换号"))?;
        let pool = source.accounts().await?;
        let current = self.inner.host.thread_profile(thread).await.unwrap_or(None);
        let next = match wanted.as_deref().map(str::trim).filter(|name| !name.is_empty()) {
            Some(name) => pool
                .iter()
                .find(|account| account.name.eq_ignore_ascii_case(name))
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("账号池里没有 {name}"))?,
            None => {
                let cooling = self.cooldowns().await?;
                accounts::choose(&pool, &cooling, now(), current.as_deref())
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("没有可换的账号：额度见底或都在冷却"))?
            }
        };
        self.inner
            .host
            .bind_profile(thread, next.name.clone())
            .await?;
        let name = next.name.clone();
        let record = self
            .change(thread_id, |record| {
                record.account = Some(name);
                record.clone()
            })
            .await?;
        Ok((record, next.label()))
    }
}
