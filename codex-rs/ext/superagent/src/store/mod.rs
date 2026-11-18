//! SQLite persistence for the domain aggregates, the key-value state and the outbox receipts
//! (prototype `src/app/store.ts`). The engine writes inside `atomic`; everything else reads.

pub mod queries;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use sqlx::AssertSqlSafe;
use sqlx::Sqlite;
use sqlx::SqliteConnection;
use sqlx::SqlitePool;
use sqlx::Transaction;

use crate::core::ChannelRef;
use crate::core::Decision;
use crate::core::Thread;
use crate::core::Ticket;
use crate::core::Trigger;
use crate::core::TriggerDisposition;
pub use queries::Timeline;
pub use queries::TriggerFilter;
pub use queries::TriggerStats;

/// What a queued message is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageKind {
    Acknowledgement,
    Reply,
    Notice,
}

/// A message for a conversation; `ticket_id` names the Ticket it is for, when there is one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboundMessage {
    pub id: String,
    pub channel: ChannelRef,
    pub text: String,
    pub kind: MessageKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
}

/// A queued message waits, went out, or was dropped because its conversation can never receive it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplyStatus {
    Pending,
    Sent,
    Dropped,
}

/// Why a message was dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DropReason {
    ChannelGone,
    ChannelIgnored,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroppedAt {
    pub at: String,
    pub reason: DropReason,
}

/// The receipt of a message, kept from the moment it is queued so an id is never queued twice.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Receipt {
    #[serde(flatten)]
    pub message: OutboundMessage,
    pub status: ReplyStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped: Option<DroppedAt>,
}

/// What `settle` records for a message: it went out, or why it was dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settle {
    Sent,
    Dropped(DropReason),
}

pub type AfterCommit = Box<dyn FnOnce() + Send + 'static>;

/// The store: one pool on `superagent_1.sqlite`. Cloning shares the pool.
#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

/// A transaction with the actions that wait for its commit. Nested `atomic` calls use savepoints;
/// a failed nested one undoes its own writes and drops its own actions, and the outer one goes on.
pub struct Tx {
    tx: Transaction<'static, Sqlite>,
    after: Vec<AfterCommit>,
    depth: u32,
}

impl Store {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Runs `work` in one transaction. A failure rolls back every write and runs none of the actions
    /// queued for its commit; a success commits, then runs them in order.
    pub async fn atomic<T>(
        &self,
        work: impl AsyncFnOnce(&mut Tx) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut tx = Tx {
            tx: self.pool.begin().await?,
            after: Vec::new(),
            depth: 0,
        };
        match work(&mut tx).await {
            Ok(value) => {
                let Tx { tx, after, .. } = tx;
                tx.commit().await?;
                for action in after {
                    action();
                }
                Ok(value)
            }
            Err(err) => {
                // A rollback that fails leaves nothing to do: the connection drops the transaction.
                let _ = tx.tx.rollback().await;
                Err(err)
            }
        }
    }

    pub async fn kv_get<V: serde::de::DeserializeOwned>(
        &self,
        key: &str,
    ) -> anyhow::Result<Option<V>> {
        queries::kv_get(&self.pool, key).await
    }

    pub async fn kv_keys(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        queries::kv_keys(&self.pool, prefix).await
    }

    pub async fn trigger(&self, id: &str) -> anyhow::Result<Option<Trigger>> {
        queries::trigger(&self.pool, id).await
    }

    pub async fn disposition(&self, id: &str) -> anyhow::Result<Option<TriggerDisposition>> {
        queries::disposition(&self.pool, id).await
    }

    pub async fn thread(&self, key: &str) -> anyhow::Result<Option<Thread>> {
        queries::thread(&self.pool, key).await
    }

    pub async fn threads(&self) -> anyhow::Result<Vec<Thread>> {
        queries::threads(&self.pool).await
    }

    /// Threads with Triggers queued for Eval or an Eval in progress.
    pub async fn threads_active(&self) -> anyhow::Result<Vec<Thread>> {
        queries::threads_active(&self.pool).await
    }

    pub async fn ticket(&self, id: &str) -> anyhow::Result<Option<Ticket>> {
        queries::ticket(&self.pool, id).await
    }

    /// Newest first.
    pub async fn tickets(&self) -> anyhow::Result<Vec<Ticket>> {
        queries::tickets(&self.pool).await
    }

    pub async fn tickets_open(&self) -> anyhow::Result<Vec<Ticket>> {
        queries::tickets_open(&self.pool).await
    }

    /// Tickets with undelivered Inputs or a pending or running Assignment.
    pub async fn tickets_active(&self) -> anyhow::Result<Vec<Ticket>> {
        queries::tickets_active(&self.pool).await
    }

    pub async fn decision(&self, id: &str) -> anyhow::Result<Option<Decision>> {
        queries::decision(&self.pool, id).await
    }

    pub async fn decisions_by_thread(&self, key: &str) -> anyhow::Result<Vec<Decision>> {
        queries::decisions_by_thread(&self.pool, key).await
    }

    pub async fn recent_triggers(&self, offset: i64, limit: i64) -> anyhow::Result<Vec<Trigger>> {
        queries::recent_triggers(&self.pool, offset, limit).await
    }

    pub async fn trigger_stats(
        &self,
        since: &str,
    ) -> anyhow::Result<std::collections::BTreeMap<String, TriggerStats>> {
        queries::trigger_stats(&self.pool, since).await
    }

    pub async fn triggers(
        &self,
        filter: &TriggerFilter,
        offset: i64,
        limit: i64,
    ) -> anyhow::Result<(Vec<Trigger>, bool)> {
        queries::triggers(&self.pool, filter, offset, limit).await
    }

    /// The latest `limit` Triggers on a Thread, oldest first, optionally before (or through) a place.
    pub async fn thread_triggers(
        &self,
        thread: &str,
        limit: i64,
        before: Option<&Timeline>,
        through: bool,
    ) -> anyhow::Result<Vec<Trigger>> {
        queries::thread_triggers(&self.pool, thread, limit, before, through).await
    }

    pub async fn thread_page(
        &self,
        thread: &str,
        offset: i64,
        limit: i64,
    ) -> anyhow::Result<Vec<Trigger>> {
        queries::thread_page(&self.pool, thread, offset, limit).await
    }

    pub async fn first_trigger(
        &self,
        thread: &str,
        human_only: bool,
    ) -> anyhow::Result<Option<Trigger>> {
        queries::first_trigger(&self.pool, thread, human_only).await
    }

    pub async fn search(&self, query: &str, limit: i64) -> anyhow::Result<Vec<Trigger>> {
        queries::search(&self.pool, query, limit).await
    }

    pub async fn recent_decisions(&self, offset: i64, limit: i64) -> anyhow::Result<Vec<Decision>> {
        queries::recent_decisions(&self.pool, offset, limit).await
    }

    pub async fn subscribed_threads(&self, source: Option<&str>) -> anyhow::Result<Vec<Thread>> {
        queries::subscribed_threads(&self.pool, source).await
    }

    pub async fn receipt(&self, id: &str) -> anyhow::Result<Option<Receipt>> {
        queries::receipt(&self.pool, id).await
    }

    pub async fn reply_status(&self, id: &str) -> anyhow::Result<Option<ReplyStatus>> {
        Ok(queries::receipt(&self.pool, id)
            .await?
            .map(|receipt| receipt.status))
    }

    pub async fn receipts_waiting(&self) -> anyhow::Result<Vec<Receipt>> {
        queries::receipts_waiting(&self.pool).await
    }

    pub async fn receipt_behind(&self, id: &str, failures: i64) -> anyhow::Result<bool> {
        queries::receipt_behind(&self.pool, id, failures).await
    }

    pub async fn failing_replies(&self) -> anyhow::Result<Vec<(Receipt, i64, String)>> {
        queries::failing_replies(&self.pool).await
    }

    pub async fn dropped_replies(&self, limit: i64) -> anyhow::Result<Vec<Receipt>> {
        queries::dropped_replies(&self.pool, limit).await
    }

    pub async fn dropped_since(&self, since: &str) -> anyhow::Result<i64> {
        queries::dropped_since(&self.pool, since).await
    }
}

impl Tx {
    /// The connection of this transaction, for the other parts' own statements (the effect queue).
    pub(crate) fn conn(&mut self) -> &mut SqliteConnection {
        &mut self.tx
    }

    /// Runs `work` in a savepoint inside this transaction.
    pub async fn atomic<T>(
        &mut self,
        work: impl AsyncFnOnce(&mut Tx) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.depth += 1;
        // The savepoint name is built from the nesting depth alone, never from caller input.
        let name = format!("superagent_atomic_{}", self.depth);
        let mark = self.after.len();
        sqlx::query(AssertSqlSafe(format!("SAVEPOINT {name}")))
            .execute(self.conn())
            .await?;
        let result = work(self).await;
        match result {
            Ok(value) => {
                sqlx::query(AssertSqlSafe(format!("RELEASE {name}")))
                    .execute(self.conn())
                    .await?;
                self.depth -= 1;
                Ok(value)
            }
            Err(err) => {
                sqlx::query(AssertSqlSafe(format!("ROLLBACK TO {name}")))
                    .execute(self.conn())
                    .await?;
                sqlx::query(AssertSqlSafe(format!("RELEASE {name}")))
                    .execute(self.conn())
                    .await?;
                self.after.truncate(mark);
                self.depth -= 1;
                Err(err)
            }
        }
    }

    /// Schedules `action` to run once the outermost transaction commits; a rollback discards it.
    /// Use it for in-memory follow-ups that need not survive a restart; durable work goes through `Effects`.
    pub fn after_commit(&mut self, action: impl FnOnce() + Send + 'static) {
        self.after.push(Box::new(action));
    }

    pub async fn kv_get<V: serde::de::DeserializeOwned>(
        &mut self,
        key: &str,
    ) -> anyhow::Result<Option<V>> {
        queries::kv_get(self.conn(), key).await
    }

    pub async fn kv_set<V: Serialize>(&mut self, key: &str, value: &V) -> anyhow::Result<()> {
        queries::kv_set(self.conn(), key, value).await
    }

    pub async fn kv_delete(&mut self, key: &str) -> anyhow::Result<()> {
        queries::kv_delete(self.conn(), key).await
    }

    pub async fn kv_keys(&mut self, prefix: &str) -> anyhow::Result<Vec<String>> {
        queries::kv_keys(self.conn(), prefix).await
    }

    pub async fn trigger(&mut self, id: &str) -> anyhow::Result<Option<Trigger>> {
        queries::trigger(self.conn(), id).await
    }

    /// Saves a Trigger and re-indexes the Threads it is on.
    pub async fn save_trigger(&mut self, trigger: &Trigger) -> anyhow::Result<()> {
        queries::save_trigger(self.conn(), trigger).await
    }

    pub async fn disposition(&mut self, id: &str) -> anyhow::Result<Option<TriggerDisposition>> {
        queries::disposition(self.conn(), id).await
    }

    pub async fn save_disposition(
        &mut self,
        id: &str,
        value: &TriggerDisposition,
    ) -> anyhow::Result<()> {
        queries::save_disposition(self.conn(), id, value).await
    }

    pub async fn thread(&mut self, key: &str) -> anyhow::Result<Option<Thread>> {
        queries::thread(self.conn(), key).await
    }

    pub async fn save_thread(&mut self, thread: &Thread) -> anyhow::Result<()> {
        queries::save_thread(self.conn(), thread).await
    }

    pub async fn threads(&mut self) -> anyhow::Result<Vec<Thread>> {
        queries::threads(self.conn()).await
    }

    pub async fn threads_active(&mut self) -> anyhow::Result<Vec<Thread>> {
        queries::threads_active(self.conn()).await
    }

    /// Sets or removes (`None`) source-specific keys in a Thread's meta; a no-op for an unknown Thread.
    pub async fn patch_thread_meta(
        &mut self,
        key: &str,
        patch: &Map<String, Value>,
    ) -> anyhow::Result<()> {
        let Some(mut thread) = self.thread(key).await? else {
            return Ok(());
        };
        let mut meta = thread.meta.clone();
        for (name, value) in patch {
            if value.is_null() {
                meta.remove(name);
            } else {
                meta.insert(name.clone(), value.clone());
            }
        }
        if meta != thread.meta {
            thread.meta = meta;
            self.save_thread(&thread).await?;
        }
        Ok(())
    }

    pub async fn ticket(&mut self, id: &str) -> anyhow::Result<Option<Ticket>> {
        queries::ticket(self.conn(), id).await
    }

    pub async fn save_ticket(&mut self, ticket: &Ticket) -> anyhow::Result<()> {
        queries::save_ticket(self.conn(), ticket).await
    }

    pub async fn tickets(&mut self) -> anyhow::Result<Vec<Ticket>> {
        queries::tickets(self.conn()).await
    }

    pub async fn tickets_open(&mut self) -> anyhow::Result<Vec<Ticket>> {
        queries::tickets_open(self.conn()).await
    }

    pub async fn tickets_active(&mut self) -> anyhow::Result<Vec<Ticket>> {
        queries::tickets_active(self.conn()).await
    }

    pub async fn decision(&mut self, id: &str) -> anyhow::Result<Option<Decision>> {
        queries::decision(self.conn(), id).await
    }

    pub async fn save_decision(&mut self, decision: &Decision) -> anyhow::Result<()> {
        queries::save_decision(self.conn(), decision).await
    }

    pub async fn decisions_by_thread(&mut self, key: &str) -> anyhow::Result<Vec<Decision>> {
        queries::decisions_by_thread(self.conn(), key).await
    }

    pub async fn receipt(&mut self, id: &str) -> anyhow::Result<Option<Receipt>> {
        queries::receipt(self.conn(), id).await
    }

    /// Records a new pending receipt; an id recorded before keeps its first receipt. Answers whether it was new.
    pub async fn receipt_add(&mut self, message: &OutboundMessage) -> anyhow::Result<bool> {
        queries::receipt_add(self.conn(), message).await
    }

    /// Marks a pending receipt as sent or dropped; recreates the receipt if the message was withdrawn
    /// while being sent, so its id cannot be queued again.
    pub async fn receipt_settle(
        &mut self,
        message: &OutboundMessage,
        outcome: Settle,
        at: &str,
    ) -> anyhow::Result<()> {
        queries::receipt_settle(self.conn(), message, outcome, at).await
    }

    /// Withdraws a pending message that has not gone out. Answers whether one was waiting.
    pub async fn receipt_withdraw(&mut self, id: &str) -> anyhow::Result<bool> {
        queries::receipt_withdraw(self.conn(), id).await
    }

    pub async fn receipt_behind(&mut self, id: &str, failures: i64) -> anyhow::Result<bool> {
        queries::receipt_behind(self.conn(), id, failures).await
    }
}
