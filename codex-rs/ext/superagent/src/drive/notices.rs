//! The drive's notices to the Router. A notice waits in the engine while its Router is in a
//! turn. Once the Router is idle, every notice waiting for it goes in one message, each checked
//! against its session's record first: a notice the Router has acted on since it was written (it
//! spoke to the session, paused it, handed it back, or released it) is dropped. So one Router
//! turn answers several sessions, and no notice reaches the Router after it answered the session
//! the notice is about.
//!
//! A session's notices go to the Router that started its drive, while that thread is still a
//! registered Router; otherwise, and for drives of earlier versions, to the first one. A notice
//! about no session goes to the Router it names: a reset card's to the first one, a low disk's to
//! the main Router (`machine.rs`).

use std::sync::Arc;
use std::time::Instant;

use chrono::DateTime;
use codex_protocol::ThreadId;
use serde_json::Value;
use serde_json::json;

use super::DriveRecord;
use super::DriveService;
use super::DriveState;
use super::KEY_PREFIX;
use super::report;
use crate::core::now_iso;
use crate::effects::EffectError;
use crate::effects::Effects;
use crate::effects::HandlerOptions;
use crate::effects::QueuedRow;
use crate::host::DeliveryMode;
use crate::store::Tx;

/// The effect kind of the drive's notices, keyed `<router>|drive:<session>[#topic]`, or
/// `<router>|<key>` for a notice about no session in particular.
pub const KIND_NOTICE: &str = "drive-notice";
/// How long a notice waits before it looks at its busy Router again.
const BUSY_RETRY_MS: i64 = 2_000;

/// What a notice is about; one waits per session and topic, the newest replacing the older.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Topic {
    /// The drive's state: a conclusion, a stop, a pause.
    State,
    /// The session moved to another account.
    Account,
    /// What the engine's round noticed (`sweep.rs`).
    Marks,
}

impl Topic {
    fn suffix(self) -> &'static str {
        match self {
            Self::State => "",
            Self::Account => "#account",
            Self::Marks => "#marks",
        }
    }
}

/// One message for the notices that wait for a Router: the notice itself when there is one, else
/// each in turn, numbered.
pub(super) fn compose(summaries: &[String]) -> String {
    match summaries {
        [one] => format!("[通知] {}", one.trim()),
        many => {
            let mut text = format!(
                "[通知] {} 路有新情况，下面逐路列出，每路分别处理：",
                many.len()
            );
            for (index, summary) in many.iter().enumerate() {
                text.push_str(&format!("\n\n【{}】{}", index + 1, summary.trim()));
            }
            text
        }
    }
}

/// Whether `later` (RFC 3339) is not before `earlier`; unreadable times count as before.
fn not_before(later: &str, earlier: &str) -> bool {
    match (
        DateTime::parse_from_rfc3339(later),
        DateTime::parse_from_rfc3339(earlier),
    ) {
        (Ok(later), Ok(earlier)) => later >= earlier,
        _ => false,
    }
}
impl DriveService {
    /// The Router a drive's notices go to: the one that started it while it is registered, else
    /// the first registered Router; `None` while there is none.
    pub(super) fn router_for(&self, record: &DriveRecord) -> Option<String> {
        let routers = (self.inner.routers)();
        record
            .router
            .clone()
            .filter(|router| routers.contains(router))
            .or_else(|| routers.into_iter().next())
    }

    /// `router` takes the drive over from the Router that drove it: the drive's notices and its
    /// line in the drives list go to `router` from now on. `None` when there is no such drive.
    pub async fn adopt(
        &self,
        thread_id: &str,
        router: &str,
    ) -> anyhow::Result<Option<DriveRecord>> {
        anyhow::ensure!(
            (self.inner.routers)().iter().any(|id| id == router),
            "{router} 不是登记的 Router"
        );
        self.change(thread_id, |record| {
            record.router = Some(router.to_owned());
            record.acted = Some(now_iso());
            record.clone()
        })
        .await
    }

    /// Every driven session whose notices go to `router`, in key order.
    pub async fn records_of(&self, router: &str) -> anyhow::Result<Vec<DriveRecord>> {
        Ok(self
            .records()
            .await?
            .into_iter()
            .filter(|record| self.router_for(record).as_deref() == Some(router))
            .collect())
    }

    /// Queues a notice about `record` for its Router; one waits per session and topic.
    pub(super) async fn queue_notice(
        &self,
        record: &DriveRecord,
        topic: Topic,
        summary: String,
    ) -> anyhow::Result<()> {
        let router = self.router_for(record).unwrap_or_default();
        let key = format!(
            "{router}|{KEY_PREFIX}{}{}",
            record.thread_id,
            topic.suffix()
        );
        let payload = json!({
            "router": router,
            "thread": record.thread_id,
            "summary": summary,
            "at": now_iso(),
        });
        self.inner
            .effects
            .enqueue_now(KIND_NOTICE, &key, &payload)
            .await
    }

    /// Queues a notice about no session in particular for the first registered Router, keyed
    /// `key` among such notices.
    pub(super) async fn tell_router(&self, key: &str, summary: String) -> anyhow::Result<()> {
        let router = (self.inner.routers)()
            .into_iter()
            .next()
            .unwrap_or_default();
        self.inner
            .store
            .atomic(async |tx| self.tell_in(tx, &router, key, summary).await)
            .await
    }

    /// Queues in `tx` a notice about no session in particular for `router` (when it is empty, the
    /// first registered Router at delivery), keyed `key` among such notices.
    pub(super) async fn tell_in(
        &self,
        tx: &mut Tx,
        router: &str,
        key: &str,
        summary: String,
    ) -> anyhow::Result<()> {
        let payload = json!({
            "router": router,
            "summary": summary,
            "at": now_iso(),
        });
        self.inner
            .effects
            .enqueue(tx, KIND_NOTICE, &format!("{router}|{key}"), &payload)
            .await
    }

    /// Tells the Router when the worker reported or the drive stopped by itself, as a notice its
    /// next turn reads; a pause the person asked for is not told back.
    pub(super) async fn notify(
        &self,
        record: &DriveRecord,
        state: &DriveState,
    ) -> anyhow::Result<()> {
        let Some(summary) = report::notice(record, state) else {
            return Ok(());
        };
        self.queue_notice(record, Topic::State, summary).await
    }

    /// Whether the Router is in a turn, or its queue waits for an account move.
    fn router_busy(&self, router: &str) -> bool {
        let watch = self.watch();
        watch.turns.contains_key(router)
            || watch
                .held
                .get(router)
                .is_some_and(|until| *until > Instant::now())
    }

    /// Whether a waiting notice still says something the Router has not acted on: its session is
    /// still driven, and the Router did nothing with it since the notice was written.
    async fn fresh(&self, payload: &Value) -> anyhow::Result<bool> {
        let Some(thread) = payload.get("thread").and_then(Value::as_str) else {
            return Ok(true);
        };
        let Some(record) = self.record(thread).await? else {
            return Ok(false);
        };
        let written = payload
            .get("at")
            .and_then(Value::as_str)
            .unwrap_or_default();
        Ok(!record
            .acted
            .as_deref()
            .is_some_and(|acted| not_before(acted, written)))
    }

    /// Delivers, in one message, what waits for `router` (the first registered Router when it is
    /// empty) once it is idle; a busy Router is asked again shortly.
    pub async fn deliver_notices(&self, router: &str) -> Result<(), EffectError> {
        let _flush = self.inner.notices.lock().await;
        let router = match router {
            "" => (self.inner.routers)()
                .into_iter()
                .next()
                .ok_or_else(|| EffectError::Later("no Router is registered".to_owned()))?,
            router => router.to_owned(),
        };
        if self.router_busy(&router) {
            return Err(EffectError::Later(format!(
                "the Router {router} is in a turn"
            )));
        }
        let failed = |err: anyhow::Error| EffectError::Failed(format!("{err:#}"));
        let rows: Vec<QueuedRow> = self
            .inner
            .effects
            .queued_rows(KIND_NOTICE)
            .await
            .map_err(failed)?
            .into_iter()
            .filter(|row| {
                let target = row.payload.get("router").and_then(Value::as_str);
                target == Some(router.as_str()) || target == Some("")
            })
            .collect();
        let mut summaries = Vec::new();
        for row in &rows {
            if self.fresh(&row.payload).await.map_err(failed)? {
                let summary = row.payload.get("summary").and_then(Value::as_str);
                summaries.extend(summary.map(str::to_owned));
            }
        }
        if !summaries.is_empty() {
            let thread = ThreadId::from_string(&router)
                .map_err(|err| EffectError::Failed(format!("Router id {router}: {err}")))?;
            self.inner
                .host
                .deliver(thread, compose(&summaries), DeliveryMode::Queue)
                .await
                .map_err(failed)?;
        }
        for row in &rows {
            self.inner
                .effects
                .cancel_unchanged(KIND_NOTICE, row)
                .await
                .map_err(failed)?;
        }
        Ok(())
    }
}

/// Registers the handler of the notices: each run delivers everything waiting for the row's
/// Router, so a row whose notice went with another one's finds nothing left to do.
pub(super) async fn install_notice_handler(
    service: &DriveService,
    effects: &Effects,
) -> anyhow::Result<()> {
    let deliverer = service.clone();
    effects
        .handle(
            KIND_NOTICE,
            Arc::new(move |_payload: Value, key: String| {
                let deliverer = deliverer.clone();
                Box::pin(async move {
                    let router = key.split_once('|').map_or("", |(router, _)| router);
                    deliverer.deliver_notices(router).await
                })
            }),
            HandlerOptions::default()
                .limit(|| Ok(4))
                .retry(BUSY_RETRY_MS, 60_000)
                .quiet(),
        )
        .await
}
