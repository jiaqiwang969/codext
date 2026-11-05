//! Startup reconciliation (design §8.1): once the engine, the executor and the PM side are up, the
//! stored work is read back and continued, oldest ticket first, without replaying anything that
//! may already have happened. The rules, as the executor's `restore` applies them:
//!
//! - a pending assignment launches again (its `launch` effect is queued once more; the effect rows
//!   themselves survived in the `effect` table), unless its ticket waits for a person after a failed
//!   launch, which stays for the person's retry;
//! - a running assignment with an `inflight` row — a delivery that never saw its end — is reported
//!   `blocked` with the recovery note and left for the person (decision #8); the row stays for
//!   inspection, and the person's retry starts a new assignment whose launch clears it;
//! - a running assignment without such a row goes to the engine as interrupted: one that was busy
//!   restarts with a recovery note and the inputs its worker did not take, oldest first; one that had
//!   settled (reported waiting, ready, blocked or failed, with nothing delivered since) keeps its
//!   thread, and only its undelivered inputs go out;
//! - the worker's thread is the one recorded for the ticket: a delivery to a cold thread resumes it
//!   first (`SuperagentHost::resume_thread` in the executor's delivery);
//! - the outcome is logged and written to the health entry `recovery`.
//!
//! # codext's own daemon recovery
//!
//! The app-server has a resume path of its own, used only when it runs as a managed daemon
//! (`app-server/src/lib.rs:534–535` sets `managed_daemon` for a unix-socket daemon launch). On a
//! daemon shutdown it snapshots the persistent root threads
//! (`request_processors/daemon_snapshot.rs:15–27`: not ephemeral, no parent, not a non-root
//! agent) with their interrupted turns (`:40–51`), and on the next start
//! `daemon_thread_recovery.rs:33` restores them through the normal cold resume and
//! `request_processors/daemon_continuation.rs:23` starts one continuation turn for each interrupted
//! one. Worker threads are persistent root threads, so they would qualify, and there is no
//! per-thread opt-out: a continuation turn on a worker whose ticket this module just blocked, or
//! restarted with a recovery note, would run beside the engine's own resume. The snapshot therefore skips the
//! threads the organization drives (`SuperagentService::owns_thread`: PM threads and the worker
//! threads of open tickets, `daemon_snapshot.rs`), and shutdown stops the engine's scheduling
//! before the snapshot is taken, so this module stays the only resume for them.

use crate::core::now_iso;
use crate::executor::CodextThreadExecutor;
use crate::health::Health;
use crate::health::HealthBoard;
use crate::store::Store;

pub use crate::executor::codext_thread::RestoreReport as Report;

/// One line for the log and the health entry.
pub fn summary(report: &Report, open: usize) -> String {
    let list = |items: &[String]| {
        if items.is_empty() {
            String::new()
        } else {
            format!("（{}）", items.join("、"))
        }
    };
    format!(
        "对账：{open} 张在途工单；重新排队 {}{}；带恢复说明重启 {}{}；有在途投递、不重放、等你处理 {}{}；等人重试 {}{}；未动 {}",
        report.relaunched.len(),
        list(&report.relaunched),
        report.restarted.len(),
        list(&report.restarted),
        report.blocked.len(),
        list(&report.blocked),
        report.waiting.len(),
        list(&report.waiting),
        report.untouched
    )
}

/// Reads the stored work back and continues it; see the module notes for the rules.
pub async fn reconcile(
    store: &Store,
    executor: &CodextThreadExecutor,
    board: &HealthBoard,
) -> anyhow::Result<Report> {
    let report = executor.restore().await?;
    let open = store.tickets_open().await?.len();
    let line = summary(&report, open);
    if report.blocked.is_empty() {
        tracing::info!("superagent: {line}");
    } else {
        tracing::warn!("superagent: {line}");
    }
    // The entry keeps the line; the handle is not needed after the one write.
    if let Err(err) = board.claim("recovery", Some(Health::ready(line, Some(now_iso())))) {
        tracing::warn!("superagent: the recovery health entry is taken: {err}");
    }
    Ok(report)
}

