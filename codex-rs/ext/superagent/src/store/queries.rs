//! The SQL behind the store: writes take a connection (they run inside a transaction), reads take
//! any executor so the engine reads inside its transaction and the views read from the pool
//! (prototype `src/app/store.ts` and `store-read.ts`).

use std::collections::BTreeMap;

use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::AssertSqlSafe;
use sqlx::Executor;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::SqliteConnection;

use super::OutboundMessage;
use super::Receipt;
use super::ReplyStatus;
use super::Settle;
use crate::core::Decision;
use crate::core::Thread;
use crate::core::Ticket;
use crate::core::Trigger;
use crate::core::TriggerDisposition;
use crate::core::thread_key;

/// A Trigger's position on its Thread's timeline: by time, then id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timeline {
    pub at: String,
    pub id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TriggerFilter {
    pub source: Option<String>,
    pub filtered: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TriggerStats {
    pub received: i64,
    pub filtered: i64,
    pub last_at: String,
}

fn parse<T: DeserializeOwned>(value: String) -> anyhow::Result<T> {
    Ok(serde_json::from_str(&value)?)
}

async fn value_rows<'e, E, T>(e: E, sql: &'static str, binds: &[&str]) -> anyhow::Result<Vec<T>>
where
    E: Executor<'e, Database = Sqlite>,
    T: DeserializeOwned,
{
    let mut query = sqlx::query(sql);
    for bind in binds {
        query = query.bind(*bind);
    }
    let rows = query.fetch_all(e).await?;
    rows.into_iter()
        .map(|row| parse(row.try_get::<String, _>("value")?))
        .collect()
}

async fn value_row<'e, E, T>(e: E, sql: &'static str, binds: &[&str]) -> anyhow::Result<Option<T>>
where
    E: Executor<'e, Database = Sqlite>,
    T: DeserializeOwned,
{
    let mut query = sqlx::query(sql);
    for bind in binds {
        query = query.bind(*bind);
    }
    match query.fetch_optional(e).await? {
        Some(row) => Ok(Some(parse(row.try_get::<String, _>("value")?)?)),
        None => Ok(None),
    }
}

/// Escapes `%`, `_` and `\` so a prefix or a search term is matched as written.
fn like_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// ---- kv ----

pub async fn kv_get<'e, E, V>(e: E, key: &str) -> anyhow::Result<Option<V>>
where
    E: Executor<'e, Database = Sqlite>,
    V: DeserializeOwned,
{
    value_row(e, "SELECT value FROM kv WHERE key = ?", &[key]).await
}

pub async fn kv_set<V: Serialize>(
    conn: &mut SqliteConnection,
    key: &str,
    value: &V,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO kv (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(key)
        .bind(serde_json::to_string(value)?)
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn kv_delete(conn: &mut SqliteConnection, key: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM kv WHERE key = ?")
        .bind(key)
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn kv_keys<'e, E>(e: E, prefix: &str) -> anyhow::Result<Vec<String>>
where
    E: Executor<'e, Database = Sqlite>,
{
    let rows = sqlx::query("SELECT key FROM kv WHERE key LIKE ? ESCAPE '\\' ORDER BY key")
        .bind(format!("{}%", like_escape(prefix)))
        .fetch_all(e)
        .await?;
    rows.into_iter()
        .map(|row| Ok(row.try_get::<String, _>("key")?))
        .collect()
}

// ---- triggers ----

pub async fn trigger<'e, E>(e: E, id: &str) -> anyhow::Result<Option<Trigger>>
where
    E: Executor<'e, Database = Sqlite>,
{
    value_row(e, "SELECT value FROM trigger WHERE id = ?", &[id]).await
}

pub async fn save_trigger(conn: &mut SqliteConnection, trigger: &Trigger) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO trigger (id, value, at) VALUES (?, ?, ?) ON CONFLICT(id) DO UPDATE SET value = excluded.value, at = excluded.at")
        .bind(&trigger.id)
        .bind(serde_json::to_string(trigger)?)
        .bind(&trigger.at)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM trigger_thread WHERE id = ?")
        .bind(&trigger.id)
        .execute(&mut *conn)
        .await?;
    for reference in &trigger.threads {
        sqlx::query(
            "INSERT OR IGNORE INTO trigger_thread (thread, id, at, human) VALUES (?, ?, ?, ?)",
        )
        .bind(thread_key(reference))
        .bind(&trigger.id)
        .bind(&trigger.at)
        .bind(i64::from(trigger.human))
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

pub async fn disposition<'e, E>(e: E, id: &str) -> anyhow::Result<Option<TriggerDisposition>>
where
    E: Executor<'e, Database = Sqlite>,
{
    value_row(e, "SELECT value FROM disposition WHERE id = ?", &[id]).await
}

pub async fn save_disposition(
    conn: &mut SqliteConnection,
    id: &str,
    value: &TriggerDisposition,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO disposition (id, value) VALUES (?, ?) ON CONFLICT(id) DO UPDATE SET value = excluded.value")
        .bind(id)
        .bind(serde_json::to_string(value)?)
        .execute(conn)
        .await?;
    Ok(())
}

// ---- threads ----

pub async fn thread<'e, E>(e: E, key: &str) -> anyhow::Result<Option<Thread>>
where
    E: Executor<'e, Database = Sqlite>,
{
    value_row(e, "SELECT value FROM thread WHERE key = ?", &[key]).await
}

pub async fn save_thread(conn: &mut SqliteConnection, thread: &Thread) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO thread (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(&thread.key)
        .bind(serde_json::to_string(thread)?)
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn threads<'e, E>(e: E) -> anyhow::Result<Vec<Thread>>
where
    E: Executor<'e, Database = Sqlite>,
{
    value_rows(e, "SELECT value FROM thread ORDER BY rowid", &[]).await
}

pub async fn threads_active<'e, E>(e: E) -> anyhow::Result<Vec<Thread>>
where
    E: Executor<'e, Database = Sqlite>,
{
    value_rows(
        e,
        "SELECT value FROM thread WHERE active = 1 ORDER BY rowid",
        &[],
    )
    .await
}

pub async fn subscribed_threads<'e, E>(e: E, source: Option<&str>) -> anyhow::Result<Vec<Thread>>
where
    E: Executor<'e, Database = Sqlite>,
{
    match source {
        Some(source) => value_rows(e, "SELECT value FROM thread WHERE subscribed = ? ORDER BY rowid", &[source]).await,
        // Force the partial index so SQLite does not scan the whole table in rowid order.
        None => value_rows(e, "SELECT value FROM thread INDEXED BY thread_subscribed WHERE subscribed IS NOT NULL ORDER BY rowid", &[]).await,
    }
}

// ---- tickets ----

pub async fn ticket<'e, E>(e: E, id: &str) -> anyhow::Result<Option<Ticket>>
where
    E: Executor<'e, Database = Sqlite>,
{
    value_row(e, "SELECT value FROM ticket WHERE id = ?", &[id]).await
}

pub async fn save_ticket(conn: &mut SqliteConnection, ticket: &Ticket) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO ticket (id, value) VALUES (?, ?) ON CONFLICT(id) DO UPDATE SET value = excluded.value")
        .bind(&ticket.id)
        .bind(serde_json::to_string(ticket)?)
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn tickets<'e, E>(e: E) -> anyhow::Result<Vec<Ticket>>
where
    E: Executor<'e, Database = Sqlite>,
{
    value_rows(e, "SELECT value FROM ticket ORDER BY rowid DESC", &[]).await
}

pub async fn tickets_open<'e, E>(e: E) -> anyhow::Result<Vec<Ticket>>
where
    E: Executor<'e, Database = Sqlite>,
{
    value_rows(
        e,
        "SELECT value FROM ticket WHERE open = 1 ORDER BY rowid DESC",
        &[],
    )
    .await
}

pub async fn tickets_active<'e, E>(e: E) -> anyhow::Result<Vec<Ticket>>
where
    E: Executor<'e, Database = Sqlite>,
{
    value_rows(
        e,
        "SELECT value FROM ticket WHERE active = 1 ORDER BY rowid DESC",
        &[],
    )
    .await
}

// ---- decisions ----

pub async fn decision<'e, E>(e: E, id: &str) -> anyhow::Result<Option<Decision>>
where
    E: Executor<'e, Database = Sqlite>,
{
    value_row(e, "SELECT value FROM decision WHERE id = ?", &[id]).await
}

pub async fn save_decision(conn: &mut SqliteConnection, decision: &Decision) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO decision (id, thread, value) VALUES (?, ?, ?) ON CONFLICT(id) DO UPDATE SET value = excluded.value, thread = excluded.thread")
        .bind(&decision.id)
        .bind(&decision.thread_key)
        .bind(serde_json::to_string(decision)?)
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn decisions_by_thread<'e, E>(e: E, key: &str) -> anyhow::Result<Vec<Decision>>
where
    E: Executor<'e, Database = Sqlite>,
{
    value_rows(
        e,
        "SELECT value FROM decision WHERE thread = ? ORDER BY rowid",
        &[key],
    )
    .await
}

pub async fn recent_decisions<'e, E>(e: E, offset: i64, limit: i64) -> anyhow::Result<Vec<Decision>>
where
    E: Executor<'e, Database = Sqlite>,
{
    let rows = sqlx::query("SELECT value FROM decision ORDER BY rowid DESC LIMIT ? OFFSET ?")
        .bind(limit)
        .bind(offset)
        .fetch_all(e)
        .await?;
    rows.into_iter()
        .map(|row| parse(row.try_get::<String, _>("value")?))
        .collect()
}

// ---- read queries ----

/// Newest first by arrival order rather than Trigger time.
pub async fn recent_triggers<'e, E>(e: E, offset: i64, limit: i64) -> anyhow::Result<Vec<Trigger>>
where
    E: Executor<'e, Database = Sqlite>,
{
    let rows = sqlx::query("SELECT value FROM trigger ORDER BY rowid DESC LIMIT ? OFFSET ?")
        .bind(limit)
        .bind(offset)
        .fetch_all(e)
        .await?;
    rows.into_iter()
        .map(|row| parse(row.try_get::<String, _>("value")?))
        .collect()
}

/// Per-source Trigger counts since `since`, plus the latest arrival across all time.
pub async fn trigger_stats<'e, E>(
    e: E,
    since: &str,
) -> anyhow::Result<BTreeMap<String, TriggerStats>>
where
    E: Executor<'e, Database = Sqlite>,
{
    let rows = sqlx::query(
        "SELECT json_extract(t.value, '$.source') AS source, MAX(t.at) AS last, SUM(t.at >= ?) AS received,
            SUM(t.at >= ? AND json_extract(d.value, '$.filtered') IS NOT NULL) AS filtered
         FROM trigger t LEFT JOIN disposition d ON d.id = t.id GROUP BY source",
    )
    .bind(since)
    .bind(since)
    .fetch_all(e)
    .await?;
    let mut result = BTreeMap::new();
    for row in rows {
        let source: String = row.try_get("source")?;
        result.insert(
            source,
            TriggerStats {
                received: row.try_get::<Option<i64>, _>("received")?.unwrap_or(0),
                filtered: row.try_get::<Option<i64>, _>("filtered")?.unwrap_or(0),
                last_at: row
                    .try_get::<Option<String>, _>("last")?
                    .unwrap_or_default(),
            },
        );
    }
    Ok(result)
}

/// Triggers newest first, optionally of one source or only the filtered ones; answers whether more follow.
pub async fn triggers<'e, E>(
    e: E,
    filter: &TriggerFilter,
    offset: i64,
    limit: i64,
) -> anyhow::Result<(Vec<Trigger>, bool)>
where
    E: Executor<'e, Database = Sqlite>,
{
    let mut conditions = Vec::new();
    if filter.source.is_some() {
        conditions.push("json_extract(t.value, '$.source') = ?");
    }
    if filter.filtered {
        conditions.push("json_extract(d.value, '$.filtered') IS NOT NULL");
    }
    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    // The clause is assembled from fixed fragments; the values are bound.
    let sql = format!(
        "SELECT t.value FROM trigger t LEFT JOIN disposition d ON d.id = t.id {where_clause} ORDER BY t.at DESC LIMIT ? OFFSET ?"
    );
    let mut query = sqlx::query(AssertSqlSafe(sql));
    if let Some(source) = &filter.source {
        query = query.bind(source.as_str());
    }
    let rows = query.bind(limit + 1).bind(offset).fetch_all(e).await?;
    let more = rows.len() as i64 > limit;
    let items = rows
        .into_iter()
        .take(limit.max(0) as usize)
        .map(|row| parse(row.try_get::<String, _>("value")?))
        .collect::<anyhow::Result<Vec<Trigger>>>()?;
    Ok((items, more))
}

/// The latest `limit` Triggers on a Thread, oldest first, optionally bounded before (or through) a place.
pub async fn thread_triggers<'e, E>(
    e: E,
    thread: &str,
    limit: i64,
    before: Option<&Timeline>,
    through: bool,
) -> anyhow::Result<Vec<Trigger>>
where
    E: Executor<'e, Database = Sqlite>,
{
    let place = match before {
        Some(_) if through => "AND (x.at, x.id) <= (?, ?)",
        Some(_) => "AND (x.at, x.id) < (?, ?)",
        None => "",
    };
    let sql = format!(
        "SELECT t.value FROM trigger_thread x JOIN trigger t ON t.id = x.id WHERE x.thread = ? {place} ORDER BY x.at DESC, x.id DESC LIMIT ?"
    );
    let mut query = sqlx::query(AssertSqlSafe(sql)).bind(thread);
    if let Some(before) = before {
        query = query.bind(before.at.as_str()).bind(before.id.as_str());
    }
    let rows = query.bind(limit).fetch_all(e).await?;
    let mut items = rows
        .into_iter()
        .map(|row| parse(row.try_get::<String, _>("value")?))
        .collect::<anyhow::Result<Vec<Trigger>>>()?;
    items.reverse();
    Ok(items)
}

pub async fn thread_page<'e, E>(
    e: E,
    thread: &str,
    offset: i64,
    limit: i64,
) -> anyhow::Result<Vec<Trigger>>
where
    E: Executor<'e, Database = Sqlite>,
{
    let rows = sqlx::query(
        "SELECT t.value FROM trigger_thread x JOIN trigger t ON t.id = x.id WHERE x.thread = ? ORDER BY x.at DESC, t.rowid DESC LIMIT ? OFFSET ?",
    )
    .bind(thread)
    .bind(limit)
    .bind(offset)
    .fetch_all(e)
    .await?;
    rows.into_iter()
        .map(|row| parse(row.try_get::<String, _>("value")?))
        .collect()
}

/// The oldest Trigger on a Thread, which names the Thread in listings.
pub async fn first_trigger<'e, E>(
    e: E,
    thread: &str,
    human_only: bool,
) -> anyhow::Result<Option<Trigger>>
where
    E: Executor<'e, Database = Sqlite>,
{
    let sql: &'static str = if human_only {
        "SELECT t.value FROM trigger_thread x JOIN trigger t ON t.id = x.id WHERE x.thread = ? AND x.human = 1 ORDER BY x.at ASC, t.rowid ASC LIMIT 1"
    } else {
        "SELECT t.value FROM trigger_thread x JOIN trigger t ON t.id = x.id WHERE x.thread = ? ORDER BY x.at ASC, t.rowid ASC LIMIT 1"
    };
    value_row(e, sql, &[thread]).await
}

/// Triggers by id, url prefix, message id or a text fragment, newest first.
pub async fn search<'e, E>(e: E, query: &str, limit: i64) -> anyhow::Result<Vec<Trigger>>
where
    E: Executor<'e, Database = Sqlite>,
{
    let escaped = like_escape(query);
    let rows = sqlx::query(
        "SELECT value FROM trigger WHERE id = ? OR json_extract(value, '$.url') LIKE ? ESCAPE '\\'
            OR json_extract(value, '$.meta.messageId') = ? OR json_extract(value, '$.text') LIKE ? ESCAPE '\\'
         ORDER BY at DESC LIMIT ?",
    )
    .bind(query)
    .bind(format!("{escaped}%"))
    .bind(query)
    .bind(format!("%{escaped}%"))
    .bind(limit)
    .fetch_all(e)
    .await?;
    rows.into_iter()
        .map(|row| parse(row.try_get::<String, _>("value")?))
        .collect()
}

// ---- receipts ----

pub async fn receipt<'e, E>(e: E, id: &str) -> anyhow::Result<Option<Receipt>>
where
    E: Executor<'e, Database = Sqlite>,
{
    value_row(e, "SELECT value FROM outbox WHERE id = ?", &[id]).await
}

/// Pending receipts, oldest first.
pub async fn receipts_waiting<'e, E>(e: E) -> anyhow::Result<Vec<Receipt>>
where
    E: Executor<'e, Database = Sqlite>,
{
    value_rows(
        e,
        "SELECT value FROM outbox WHERE value ->> '$.status' = 'pending' ORDER BY rowid",
        &[],
    )
    .await
}

/// Whether an earlier message for the same ticket and conversation still waits with fewer than
/// `failures` consecutive failed `reply` attempts.
pub async fn receipt_behind<'e, E>(e: E, id: &str, failures: i64) -> anyhow::Result<bool>
where
    E: Executor<'e, Database = Sqlite>,
{
    let row = sqlx::query(
        "SELECT 1 AS found FROM outbox earlier JOIN outbox message ON message.id = ?
            LEFT JOIN effect ON effect.kind = 'reply' AND effect.key = earlier.id
         WHERE earlier.pending = message.pending AND earlier.rowid < message.rowid AND IFNULL(effect.attempts, 0) < ? LIMIT 1",
    )
    .bind(id)
    .bind(failures)
    .fetch_optional(e)
    .await?;
    Ok(row.is_some())
}

pub async fn receipt_add(
    conn: &mut SqliteConnection,
    message: &OutboundMessage,
) -> anyhow::Result<bool> {
    let receipt = Receipt {
        message: message.clone(),
        status: ReplyStatus::Pending,
        dropped: None,
    };
    let result = sqlx::query("INSERT OR IGNORE INTO outbox (id, value) VALUES (?, ?)")
        .bind(&message.id)
        .bind(serde_json::to_string(&receipt)?)
        .execute(conn)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn receipt_settle(
    conn: &mut SqliteConnection,
    message: &OutboundMessage,
    outcome: Settle,
    at: &str,
) -> anyhow::Result<()> {
    let (status, dropped) = match outcome {
        Settle::Sent => (ReplyStatus::Sent, None),
        Settle::Dropped(reason) => (
            ReplyStatus::Dropped,
            Some(super::DroppedAt {
                at: at.to_owned(),
                reason,
            }),
        ),
    };
    let patch = serde_json::json!({ "status": status, "dropped": dropped });
    let updated = sqlx::query("UPDATE outbox SET value = json_patch(value, ?) WHERE id = ? AND value ->> '$.status' = 'pending'")
        .bind(serde_json::to_string(&patch)?)
        .bind(&message.id)
        .execute(&mut *conn)
        .await?;
    if updated.rows_affected() == 0 {
        let receipt = Receipt {
            message: message.clone(),
            status,
            dropped,
        };
        sqlx::query("INSERT OR IGNORE INTO outbox (id, value) VALUES (?, ?)")
            .bind(&message.id)
            .bind(serde_json::to_string(&receipt)?)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

pub async fn receipt_withdraw(conn: &mut SqliteConnection, id: &str) -> anyhow::Result<bool> {
    let result =
        sqlx::query("DELETE FROM outbox WHERE id = ? AND value ->> '$.status' = 'pending'")
            .bind(id)
            .execute(conn)
            .await?;
    Ok(result.rows_affected() > 0)
}

/// Messages whose `reply` effect failed, with their consecutive failures and last error, oldest first.
pub async fn failing_replies<'e, E>(e: E) -> anyhow::Result<Vec<(Receipt, i64, String)>>
where
    E: Executor<'e, Database = Sqlite>,
{
    let rows = sqlx::query(
        "SELECT o.value, e.attempts, e.error FROM effect e JOIN outbox o ON o.id = e.key
         WHERE e.kind = 'reply' AND e.attempts > 0 ORDER BY e.seq",
    )
    .fetch_all(e)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok((
                parse(row.try_get::<String, _>("value")?)?,
                row.try_get::<i64, _>("attempts")?,
                row.try_get::<String, _>("error")?,
            ))
        })
        .collect()
}

pub async fn dropped_replies<'e, E>(e: E, limit: i64) -> anyhow::Result<Vec<Receipt>>
where
    E: Executor<'e, Database = Sqlite>,
{
    let rows = sqlx::query(
        "SELECT value FROM outbox WHERE dropped IS NOT NULL ORDER BY dropped DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(e)
    .await?;
    rows.into_iter()
        .map(|row| parse(row.try_get::<String, _>("value")?))
        .collect()
}

pub async fn dropped_since<'e, E>(e: E, since: &str) -> anyhow::Result<i64>
where
    E: Executor<'e, Database = Sqlite>,
{
    let row = sqlx::query("SELECT COUNT(*) AS n FROM outbox WHERE dropped >= ?")
        .bind(since)
        .fetch_one(e)
        .await?;
    Ok(row.try_get::<i64, _>("n")?)
}
