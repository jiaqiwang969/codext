//! Opening the optional organization ledger through the shared SQLite policy.

use crate::SqliteConfig;
use crate::migrations::runtime_superagent_migrator;
use sqlx_sqlite::SqlitePool;

/// Open the authoritative organization ledger only when its coordinator is enabled.
pub async fn open_superagent_db(sqlite: &SqliteConfig) -> anyhow::Result<SqlitePool> {
    sqlite
        .open_superagent_db(&runtime_superagent_migrator())
        .await
}
