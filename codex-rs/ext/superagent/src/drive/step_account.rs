//! The account of a driven session: checked before each step, and named by the Router when it
//! starts a drive.

use std::path::Path;

use codex_protocol::ThreadId;

use super::DriveService;
use super::accounts::Activity;
use super::accounts::Plan;
use super::pool::Look;
use super::pool::Verdict;

/// The ledger key of the drive steps whose account the engine already moved.
pub(super) const STEP_MOVED_KEY: &str = "drive-step-moved";

/// The claim of step `n` of the drive started at `since` on a session.
pub(super) fn step_key(thread_id: &str, since: &str, n: u32) -> String {
    format!("{thread_id}@{since}#{n}")
}

impl DriveService {
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

    /// Before step `n` goes to a driven session: the session's own account is read (cheap,
    /// usually already known); when it runs low (below the soft threshold, cooling down or not
    /// usable), the session moves to the best account of the pool first, and the drive records
    /// the account as `switch_account` does. A session whose turn runs (the person typed into
    /// it) is left alone: a step never moves midway. A failed check is logged and the step goes
    /// out all the same.
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
            let standing = self.standing(&current, Look::Recent).await?;
            if standing.healthy() {
                self.unstuck(thread_id);
                return Ok(());
            }
            let Verdict { plan, why } = self.plan_move(standing, Activity::Idle).await?;
            match plan {
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
