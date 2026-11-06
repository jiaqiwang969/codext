//! The durable effect queue (prototype `src/app/effects.ts`): a command enqueues what it wants done
//! inside its transaction, and the queue runs it after the commit, per kind, with limits, retries
//! with backoff, order where a kind needs it, and a health entry per kind.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use chrono::DateTime;
use chrono::SecondsFormat;
use chrono::Utc;
use serde::Serialize;
use serde_json::Value;
use sqlx::Row;
use tokio::sync::Notify;
use tokio::sync::watch;

use crate::BoxFut;
use crate::health::Health;
use crate::health::HealthBoard;
use crate::health::HealthEntry;
use crate::store::Store;
use crate::store::Tx;

/// Consecutive failures before an effect kind reports an error.
pub const FAILURES_BEFORE_ERROR: i64 = 3;

/// Milliseconds since the epoch; injectable for tests.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| Utc::now().timestamp_millis())
}

/// A time in the one form the store compares, from milliseconds since the epoch.
pub fn iso(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_default()
}

/// Why a handler did not finish: it cannot run right now (`Later`, which costs no attempt, such as
/// while processing is paused or no worker slot is free), or it failed.
#[derive(Debug, thiserror::Error)]
pub enum EffectError {
    #[error("{0}")]
    Later(String),
    #[error("{0}")]
    Failed(String),
}

impl From<anyhow::Error> for EffectError {
    fn from(err: anyhow::Error) -> Self {
        Self::Failed(format!("{err:#}"))
    }
}

/// Runs one effect: its payload and its key.
pub type EffectRun =
    Arc<dyn Fn(Value, String) -> BoxFut<'static, Result<(), EffectError>> + Send + Sync>;
/// How many effects of a kind may run at once now; 0 holds the kind.
pub type Limit = Arc<dyn Fn() -> Result<usize, String> + Send + Sync>;

pub struct HandlerOptions {
    pub limit: Limit,
    /// Initial and maximum retry delays, in milliseconds.
    pub retry: (i64, i64),
    /// Effects run one at a time in queue order, and a failed one holds back the later ones.
    pub ordered: bool,
    /// Repeated failures do not mark the kind's health as error (they show elsewhere, such as on a ticket).
    pub quiet: bool,
}

impl Default for HandlerOptions {
    fn default() -> Self {
        Self {
            limit: Arc::new(|| Ok(1)),
            retry: (2_000, 60_000),
            ordered: false,
            quiet: false,
        }
    }
}

impl HandlerOptions {
    pub fn limit(
        mut self,
        limit: impl Fn() -> Result<usize, String> + Send + Sync + 'static,
    ) -> Self {
        self.limit = Arc::new(limit);
        self
    }

    pub fn retry(mut self, initial: i64, maximum: i64) -> Self {
        self.retry = (initial, maximum);
        self
    }

    pub fn ordered(mut self) -> Self {
        self.ordered = true;
        self
    }

    pub fn quiet(mut self) -> Self {
        self.quiet = true;
        self
    }
}

struct Handler {
    run: EffectRun,
    limit: Limit,
    retry: (i64, i64),
    ordered: bool,
    quiet: bool,
}

#[derive(Clone, Debug)]
struct EffectRow {
    key: String,
    value: String,
    seq: i64,
    generation: i64,
    attempts: i64,
    due: i64,
}

struct Inner {
    store: Store,
    board: HealthBoard,
    clock: Clock,
    handlers: Mutex<BTreeMap<String, Arc<Handler>>>,
    health: Mutex<HashMap<String, HealthEntry>>,
    /// The effects running now, by kind and key.
    running: Mutex<HashSet<(String, String)>>,
    /// Kinds whose last dispatch failed; the next that succeeds refreshes their health.
    broken: Mutex<HashSet<String>>,
    seq: AtomicI64,
    started: AtomicBool,
    stopped: AtomicBool,
    looping: AtomicBool,
    kick: Notify,
    /// Bumps whenever an effect finishes, for `idle` and `drain`.
    progress: watch::Sender<u64>,
    /// The dispatcher's rounds: counted when one starts, before it reads the queue, and when it
    /// completes. Only the dispatcher task dispatches, so no lock is held across the queue reads.
    rounds_started: AtomicU64,
    rounds: watch::Sender<u64>,
}

#[derive(Clone)]
pub struct Effects {
    inner: Arc<Inner>,
}

impl Effects {
    pub async fn new(store: Store, board: HealthBoard, clock: Clock) -> anyhow::Result<Self> {
        let row = sqlx::query("SELECT IFNULL(MAX(seq), 0) AS seq FROM effect")
            .fetch_one(store.pool())
            .await?;
        let seq: i64 = row.try_get("seq")?;
        let (progress, _) = watch::channel(0u64);
        let (rounds, _) = watch::channel(0u64);
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                board,
                clock,
                handlers: Mutex::new(BTreeMap::new()),
                health: Mutex::new(HashMap::new()),
                running: Mutex::new(HashSet::new()),
                broken: Mutex::new(HashSet::new()),
                seq: AtomicI64::new(seq),
                started: AtomicBool::new(false),
                stopped: AtomicBool::new(false),
                looping: AtomicBool::new(false),
                kick: Notify::new(),
                progress,
                rounds_started: AtomicU64::new(0),
                rounds,
            }),
        })
    }

    fn now(&self) -> i64 {
        (self.inner.clock)()
    }

    /// Upserts an effect in the caller's transaction, coalescing with a waiting row of the same kind
    /// and key while keeping its queue position; the queue is kicked once the transaction commits.
    pub async fn enqueue<P: Serialize>(
        &self,
        tx: &mut Tx,
        kind: &str,
        key: &str,
        payload: &P,
    ) -> anyhow::Result<()> {
        let now = self.now();
        let seq = self.inner.seq.fetch_add(1, Ordering::SeqCst) + 1;
        sqlx::query(
            "INSERT INTO effect (kind, key, value, seq, due, since) VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(kind, key) DO UPDATE SET value = excluded.value, generation = generation + 1, attempts = 0, error = '', due = excluded.due",
        )
        .bind(kind)
        .bind(key)
        .bind(serde_json::to_string(payload)?)
        .bind(seq)
        .bind(now)
        .bind(iso(now))
        .execute(tx.conn())
        .await?;
        let effects = self.clone();
        tx.after_commit(move || effects.kick());
        Ok(())
    }

    /// Enqueues outside any command, in a transaction of its own.
    pub async fn enqueue_now<P: Serialize>(
        &self,
        kind: &str,
        key: &str,
        payload: &P,
    ) -> anyhow::Result<()> {
        let payload = serde_json::to_value(payload)?;
        self.inner
            .store
            .atomic(async |tx| self.enqueue(tx, kind, key, &payload).await)
            .await
    }

    /// Like `enqueue_now`, but due only after `delay`; the queue is kicked again then.
    pub async fn enqueue_after<P: Serialize>(
        &self,
        kind: &str,
        key: &str,
        payload: &P,
        delay: std::time::Duration,
    ) -> anyhow::Result<()> {
        let payload = serde_json::to_value(payload)?;
        self.inner.store.atomic(async |tx| {
            self.enqueue_after_in(tx, kind, key, &payload, delay).await
        }).await
    }

    /// Enqueues delayed work in the command's transaction, alongside its deduplication receipt.
    pub async fn enqueue_after_in<P: Serialize>(
        &self,
        tx: &mut Tx,
        kind: &str,
        key: &str,
        payload: &P,
        delay: std::time::Duration,
    ) -> anyhow::Result<()> {
        let due = self.now() + delay.as_millis() as i64;
        let seq = self.inner.seq.fetch_add(1, Ordering::SeqCst) + 1;
        sqlx::query(
            "INSERT INTO effect (kind, key, value, seq, due, since) VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(kind, key) DO UPDATE SET value = excluded.value, generation = generation + 1, attempts = 0, error = '', due = excluded.due",
        )
        .bind(kind)
        .bind(key)
        .bind(serde_json::to_string(payload)?)
        .bind(seq)
        .bind(due)
        .bind(iso(due))
        .execute(tx.conn())
        .await?;
        let effects = self.clone();
        tx.after_commit(move || {
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                effects.kick();
            });
        });
        Ok(())
    }

    /// Registers the handler of a kind; a kind has one handler.
    pub async fn handle(
        &self,
        kind: &str,
        run: EffectRun,
        options: HandlerOptions,
    ) -> anyhow::Result<()> {
        {
            let handlers = self
                .inner
                .handlers
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if handlers.contains_key(kind) {
                anyhow::bail!("The {kind} effects already have a handler");
            }
        }
        let entry = self.inner.board.claim(kind, None)?;
        self.inner
            .health
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(kind.to_owned(), entry);
        let limit = if options.ordered {
            let inner = options.limit;
            Arc::new(move || inner().map(|n| n.min(1))) as Limit
        } else {
            options.limit
        };
        self.inner
            .handlers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                kind.to_owned(),
                Arc::new(Handler {
                    run,
                    limit,
                    retry: options.retry,
                    ordered: options.ordered,
                    quiet: options.quiet,
                }),
            );
        self.report(kind).await;
        self.kick();
        Ok(())
    }

    /// Whether an effect of the kind, or the one with `key`, waits or runs.
    pub async fn pending(&self, kind: &str, key: Option<&str>) -> anyhow::Result<bool> {
        let row = match key {
            Some(key) => {
                sqlx::query("SELECT 1 AS one FROM effect WHERE kind = ? AND key = ?")
                    .bind(kind)
                    .bind(key)
                    .fetch_optional(self.inner.store.pool())
                    .await?
            }
            None => {
                sqlx::query("SELECT 1 AS one FROM effect WHERE kind = ? LIMIT 1")
                    .bind(kind)
                    .fetch_optional(self.inner.store.pool())
                    .await?
            }
        };
        Ok(row.is_some())
    }

    /// The queued effects of a kind, oldest first, as (key, payload).
    pub async fn queued(&self, kind: &str) -> anyhow::Result<Vec<(String, Value)>> {
        let rows = sqlx::query("SELECT key, value FROM effect WHERE kind = ? ORDER BY seq")
            .bind(kind)
            .fetch_all(self.inner.store.pool())
            .await?;
        rows.into_iter()
            .map(|row| {
                let key: String = row.try_get("key")?;
                let value: String = row.try_get("value")?;
                Ok((key, serde_json::from_str(&value).unwrap_or(Value::Null)))
            })
            .collect()
    }

    /// Drops a queued row; if it is running, that attempt finishes normally.
    pub async fn cancel(&self, tx: &mut Tx, kind: &str, key: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM effect WHERE kind = ? AND key = ?")
            .bind(kind)
            .bind(key)
            .execute(tx.conn())
            .await?;
        let effects = self.clone();
        let kind = kind.to_owned();
        tx.after_commit(move || {
            tokio::spawn(async move { effects.report(&kind).await });
        });
        Ok(())
    }

    /// Effects run only between `start` and `drain`; rows queued before wait.
    pub fn start(&self) {
        self.inner.started.store(true, Ordering::SeqCst);
        if !self.inner.looping.swap(true, Ordering::SeqCst) {
            let effects = self.clone();
            tokio::spawn(async move {
                loop {
                    effects.inner.kick.notified().await;
                    if effects.inner.stopped.load(Ordering::SeqCst) {
                        // Wake whoever waits for a round, so it sees the stop.
                        effects.inner.rounds.send_modify(|rounds| *rounds += 1);
                        return;
                    }
                    effects.inner.rounds_started.fetch_add(1, Ordering::SeqCst);
                    effects.dispatch().await;
                    effects.inner.rounds.send_modify(|rounds| *rounds += 1);
                }
            });
        }
        self.kick();
    }

    pub fn kick(&self) {
        self.inner.kick.notify_one();
    }

    /// Asks the dispatcher for a round and waits for one that started after this request, so it
    /// read everything committed before.
    async fn round(&self) {
        if !self.inner.started.load(Ordering::SeqCst) || self.inner.stopped.load(Ordering::SeqCst) {
            return;
        }
        let mut rounds = self.inner.rounds.subscribe();
        let started = self.inner.rounds_started.load(Ordering::SeqCst);
        self.kick();
        let stopped = &self.inner.stopped;
        let _ = rounds
            .wait_for(|completed| *completed > started || stopped.load(Ordering::SeqCst))
            .await;
    }

    /// Resolves once no effect of `kind` (or of any kind) runs and none can start: none is due, or
    /// the kind's limit has no room.
    pub async fn idle(&self, kind: Option<&str>) {
        let mut progress = self.inner.progress.subscribe();
        loop {
            let seen = *progress.borrow_and_update();
            self.round().await;
            // An effect that finished while that round read the queue may have freed room for a
            // waiting row the round skipped; run another before concluding that nothing can start.
            if *progress.borrow() != seen {
                continue;
            }
            if !self.running(kind) {
                return;
            }
            let _ = progress.wait_for(|value| *value != seen).await;
        }
    }

    /// Stops starting new effects and waits for the running ones; waiting rows stay for the next start.
    pub async fn drain(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        self.kick();
        let mut progress = self.inner.progress.subscribe();
        loop {
            let seen = *progress.borrow_and_update();
            if !self.running(None) {
                return;
            }
            let _ = progress.wait_for(|value| *value != seen).await;
        }
    }

    fn running(&self, kind: Option<&str>) -> bool {
        let running = self
            .inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match kind {
            Some(kind) => running.iter().any(|(k, _)| k == kind),
            None => !running.is_empty(),
        }
    }

    /// One round over every kind; only the dispatcher task calls it.
    async fn dispatch(&self) {
        if !self.inner.started.load(Ordering::SeqCst) || self.inner.stopped.load(Ordering::SeqCst) {
            return;
        }
        let handlers: Vec<(String, Arc<Handler>)> = self
            .inner
            .handlers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(kind, handler)| (kind.clone(), Arc::clone(handler)))
            .collect();
        for (kind, handler) in handlers {
            if let Err(err) = self.dispatch_kind(&kind, &handler).await {
                self.failed(&kind, &err);
                continue;
            }
            let was_broken = self
                .inner
                .broken
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&kind);
            if was_broken {
                self.report(&kind).await;
            }
        }
    }

    async fn dispatch_kind(&self, kind: &str, handler: &Arc<Handler>) -> anyhow::Result<()> {
        let now = self.now();
        let running: HashSet<String> = self
            .inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(k, _)| k == kind)
            .map(|(_, key)| key.clone())
            .collect();
        // Ordered kinds wait behind their oldest row even when it is not due yet.
        let rows = if handler.ordered {
            sqlx::query("SELECT key, value, seq, generation, attempts, due FROM effect WHERE kind = ? ORDER BY seq LIMIT 1")
                .bind(kind)
                .fetch_all(self.inner.store.pool())
                .await?
        } else {
            sqlx::query("SELECT key, value, seq, generation, attempts, due FROM effect WHERE kind = ? AND due <= ? ORDER BY seq LIMIT ?")
                .bind(kind)
                .bind(now)
                .bind(16 + running.len() as i64)
                .fetch_all(self.inner.store.pool())
                .await?
        };
        let rows: Vec<EffectRow> = rows
            .into_iter()
            .map(|row| {
                Ok(EffectRow {
                    key: row.try_get("key")?,
                    value: row.try_get("value")?,
                    seq: row.try_get("seq")?,
                    generation: row.try_get("generation")?,
                    attempts: row.try_get("attempts")?,
                    due: row.try_get("due")?,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let ready: Vec<EffectRow> = if handler.ordered && !running.is_empty() {
            Vec::new()
        } else {
            rows.into_iter()
                .filter(|row| row.due <= now && !running.contains(&row.key))
                .collect()
        };
        // The limit is asked only once ready rows exist.
        let mut room = if ready.is_empty() {
            0
        } else {
            (handler.limit)()
                .map_err(|err| anyhow::anyhow!(err))?
                .saturating_sub(running.len())
        };
        for row in ready {
            if room == 0 {
                break;
            }
            room -= 1;
            self.spawn(kind, handler, row);
        }
        Ok(())
    }

    fn spawn(&self, kind: &str, handler: &Arc<Handler>, row: EffectRow) {
        let task = (kind.to_owned(), row.key.clone());
        self.inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(task.clone());
        let effects = self.clone();
        let handler = Arc::clone(handler);
        let kind = kind.to_owned();
        tokio::spawn(async move {
            if let Err(err) = effects.attempt(&kind, &handler, row).await {
                effects.failed(&kind, &err);
            }
            effects
                .inner
                .running
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&task);
            effects.inner.progress.send_modify(|value| *value += 1);
            effects.kick();
        });
    }

    async fn attempt(
        &self,
        kind: &str,
        handler: &Arc<Handler>,
        row: EffectRow,
    ) -> anyhow::Result<()> {
        let payload: Value = serde_json::from_str(&row.value).unwrap_or(Value::Null);
        let outcome = (handler.run)(payload, row.key.clone()).await;
        let pool = self.inner.store.pool();
        // Match seq and generation so a row replaced or queued again while running is not overwritten.
        match outcome {
            Ok(()) => {
                sqlx::query(
                    "DELETE FROM effect WHERE kind = ? AND key = ? AND seq = ? AND generation = ?",
                )
                .bind(kind)
                .bind(&row.key)
                .bind(row.seq)
                .bind(row.generation)
                .execute(pool)
                .await?;
            }
            // Leave running rows untouched on shutdown so they retry on the next start.
            Err(_) if self.inner.stopped.load(Ordering::SeqCst) => return Ok(()),
            Err(EffectError::Later(_)) => {
                sqlx::query("UPDATE effect SET due = ? WHERE kind = ? AND key = ? AND seq = ? AND generation = ?")
                    .bind(self.now() + handler.retry.0)
                    .bind(kind)
                    .bind(&row.key)
                    .bind(row.seq)
                    .bind(row.generation)
                    .execute(pool)
                    .await?;
            }
            Err(EffectError::Failed(error)) => {
                let exponent = row.attempts.clamp(0, 16) as u32;
                let delay = handler
                    .retry
                    .1
                    .min(handler.retry.0.saturating_mul(1i64 << exponent));
                let error: String = error.chars().take(1000).collect();
                sqlx::query("UPDATE effect SET attempts = attempts + 1, error = ?, due = ? WHERE kind = ? AND key = ? AND seq = ? AND generation = ?")
                    .bind(error)
                    .bind(self.now() + delay)
                    .bind(kind)
                    .bind(&row.key)
                    .bind(row.seq)
                    .bind(row.generation)
                    .execute(pool)
                    .await?;
            }
        }
        if !self.inner.stopped.load(Ordering::SeqCst) {
            self.report(kind).await;
        }
        Ok(())
    }

    fn failed(&self, kind: &str, err: &anyhow::Error) {
        self.inner
            .broken
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(kind.to_owned());
        if let Some(entry) = self.entry(kind) {
            entry.set(Health::error(
                format!("The effect queue failed: {err:#}"),
                Some(iso(self.now())),
            ));
        }
    }

    fn entry(&self, kind: &str) -> Option<HealthEntry> {
        self.inner
            .health
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(kind)
            .cloned()
    }

    async fn report(&self, kind: &str) {
        let Some(entry) = self.entry(kind) else {
            return;
        };
        let quiet = self
            .inner
            .handlers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(kind)
            .is_some_and(|handler| handler.quiet);
        let row = sqlx::query(
            "SELECT COUNT(*) AS waiting, MIN(since) AS since, MAX(attempts) AS attempts,
                (SELECT error FROM effect WHERE kind = ? AND attempts > 0 ORDER BY seq DESC LIMIT 1) AS error
             FROM effect WHERE kind = ?",
        )
        .bind(kind)
        .bind(kind)
        .fetch_one(self.inner.store.pool())
        .await;
        let at = Some(iso(self.now()));
        let row = match row {
            Ok(row) => row,
            Err(err) => {
                entry.set(Health::error(format!("The effect queue failed: {err}"), at));
                return;
            }
        };
        let waiting: i64 = row.try_get::<i64, _>("waiting").unwrap_or(0);
        let since: Option<String> = row.try_get("since").unwrap_or(None);
        let attempts: i64 = row
            .try_get::<Option<i64>, _>("attempts")
            .ok()
            .flatten()
            .unwrap_or(0);
        let error: Option<String> = row.try_get("error").unwrap_or(None);
        let detail = if waiting > 0 {
            format!("{waiting} waiting since {}", since.unwrap_or_default())
        } else {
            "Nothing waiting".to_owned()
        };
        if attempts >= FAILURES_BEFORE_ERROR && !quiet {
            entry.set(Health::error(
                format!("{detail}; the last failure: {}", error.unwrap_or_default()),
                at,
            ));
        } else {
            entry.set(Health::ready(detail, at));
        }
    }
}
