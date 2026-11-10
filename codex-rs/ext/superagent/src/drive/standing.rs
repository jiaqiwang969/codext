//! The person's standing rules: things the person told the Router that hold beyond one step,
//! such as how many sessions an account carries or that a branch may be pushed to the person's
//! fork. The Router keeps one with `remember`, word for word (the engine checks the person said
//! it), for every session, for the sessions under a folder, or for one session; `forget` drops
//! it. They live in the ledger, so a Router whose history was compacted still has them: its
//! context lists them (`router.rs`), and a step may quote them as what it follows from or what it
//! serves (`basis.rs`).

use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use super::DriveRecord;
use super::DriveService;
use crate::core::now_iso;

/// The ledger key of the standing rules.
pub const STANDING_KEY: &str = "drive-standing";
/// The most rules kept; the oldest go first.
const STANDING: usize = 50;

/// Which sessions a standing rule holds for.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StandingScope {
    /// Every session.
    All,
    /// The sessions whose folder is this one or lies under it.
    Folder { path: PathBuf },
    /// One session.
    Session { thread: String },
}

/// One standing rule: the person's words and where they hold.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StandingRule {
    /// `r1`, `r2`, …: how the Router names it to `forget` it.
    pub id: String,
    /// The person's words, as they said them.
    pub text: String,
    pub scope: StandingScope,
    /// RFC 3339, UTC.
    pub at: String,
}

impl StandingRule {
    /// Whether the rule holds for the session of `record`.
    pub fn holds_for(&self, record: &DriveRecord) -> bool {
        match &self.scope {
            StandingScope::All => true,
            StandingScope::Folder { path } => record.project_home.starts_with(path),
            StandingScope::Session { thread } => *thread == record.thread_id,
        }
    }

    /// One line for the Router: `[r1] 全部：…`.
    pub fn line(&self) -> String {
        let scope = match &self.scope {
            StandingScope::All => "全部".to_owned(),
            StandingScope::Folder { path } => path.display().to_string(),
            StandingScope::Session { thread } => format!("会话 {thread}"),
        };
        format!("[{}] {scope}：{}", self.id, self.text)
    }
}

impl DriveService {
    /// Every standing rule, oldest first.
    pub async fn standing_rules(&self) -> anyhow::Result<Vec<StandingRule>> {
        Ok(self
            .inner
            .store
            .kv_get(STANDING_KEY)
            .await?
            .unwrap_or_default())
    }

    /// Keeps the person's words as a standing rule for `scope`; they must be (part of) something
    /// the person said to the Router. The same words for the same scope are kept once.
    pub async fn remember(&self, text: &str, scope: StandingScope) -> anyhow::Result<StandingRule> {
        let text = text.trim().to_owned();
        anyhow::ensure!(!text.is_empty(), "要记住的话（text）是空的");
        self.ensure_heard(&text).await?;
        self.inner
            .store
            .atomic(async |tx| {
                let mut rules: Vec<StandingRule> =
                    tx.kv_get(STANDING_KEY).await?.unwrap_or_default();
                if let Some(rule) = rules
                    .iter()
                    .find(|rule| rule.text == text && rule.scope == scope)
                {
                    return Ok(rule.clone());
                }
                let next = rules
                    .iter()
                    .filter_map(|rule| rule.id.strip_prefix('r')?.parse::<u32>().ok())
                    .max()
                    .unwrap_or(0)
                    + 1;
                let rule = StandingRule {
                    id: format!("r{next}"),
                    text,
                    scope,
                    at: now_iso(),
                };
                rules.push(rule.clone());
                let excess = rules.len().saturating_sub(STANDING);
                rules.drain(..excess);
                tx.kv_set(STANDING_KEY, &rules).await?;
                Ok(rule)
            })
            .await
    }

    /// Drops the standing rule `id`; answers it, or `None` when there is none.
    pub async fn forget(&self, id: &str) -> anyhow::Result<Option<StandingRule>> {
        let id = id.trim().to_owned();
        self.inner
            .store
            .atomic(async |tx| {
                let mut rules: Vec<StandingRule> =
                    tx.kv_get(STANDING_KEY).await?.unwrap_or_default();
                let Some(index) = rules.iter().position(|rule| rule.id == id) else {
                    return Ok(None);
                };
                let rule = rules.remove(index);
                tx.kv_set(STANDING_KEY, &rules).await?;
                Ok(Some(rule))
            })
            .await
    }

    /// The words of the standing rules that hold for the session of `record`.
    pub(super) async fn standing_for(&self, record: &DriveRecord) -> anyhow::Result<Vec<String>> {
        Ok(self
            .standing_rules()
            .await?
            .into_iter()
            .filter(|rule| rule.holds_for(record))
            .map(|rule| rule.text)
            .collect())
    }
}
