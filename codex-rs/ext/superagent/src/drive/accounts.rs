//! Account switching for driven sessions. When a step's turn fails because its account ran out of
//! quota (`usageLimitExceeded`) or the model is at capacity on it (`serverOverloaded`, "Selected
//! model is at capacity"), that account cools down and the step goes again on another account,
//! chosen the way `codex-account-next list` orders the pool: the account whose quota resets
//! soonest first, since quota left at a reset is lost. Free accounts, accounts with too little
//! quota left and accounts still cooling down are skipped.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::BoxFut;
use crate::host::TurnBrief;

/// The ledger key of the accounts cooling down.
pub const COOLDOWN_KEY: &str = "drive-account-cooldowns";
/// The least quota (percent) an account needs to take a step.
pub const MIN_REMAINING: f64 = 5.0;
/// How long an account cools down after the model was at capacity on it.
pub const CAPACITY_COOLDOWN: Duration = Duration::from_secs(15 * 60);
/// How long an out-of-quota account cools down when its reset time is unknown.
pub const QUOTA_COOLDOWN: Duration = Duration::from_secs(60 * 60);
const HELPER_TIMEOUT: Duration = Duration::from_secs(150);

/// Why a step's account could not carry it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trouble {
    /// The account's quota is used up.
    UsageLimit,
    /// The model is at capacity on this account.
    Capacity,
}

impl Trouble {
    /// In the person's words.
    pub fn describe(self) -> &'static str {
        match self {
            Self::UsageLimit => "账号额度用完",
            Self::Capacity => "模型满载（Selected model is at capacity）",
        }
    }
}

/// The trouble a failed turn reports, if it is about the account rather than the work.
pub fn trouble(turn: &TurnBrief) -> Option<Trouble> {
    match turn.error_kind.as_deref() {
        Some("usageLimitExceeded") => Some(Trouble::UsageLimit),
        Some("serverOverloaded") => Some(Trouble::Capacity),
        Some(_) | None => turn
            .error
            .as_deref()
            .filter(|message| message.contains("at capacity"))
            .map(|_| Trouble::Capacity),
    }
}

/// One account of the pool with its live quota.
#[derive(Clone, Debug, PartialEq)]
pub struct AccountInfo {
    pub name: String,
    pub plan: String,
    /// Logged in, reachable and without errors.
    pub usable: bool,
    /// The least quota left over its windows, in percent.
    pub remaining: f64,
    /// The soonest reset over its windows (Unix seconds).
    pub resets_at: Option<i64>,
}

impl AccountInfo {
    /// One line for the person: `name（剩 84%，11-05 19:19 重置）`.
    pub fn label(&self) -> String {
        let reset = self
            .resets_at
            .and_then(|at| chrono::DateTime::from_timestamp(at, 0))
            .map(|at| {
                at.with_timezone(&chrono::Local)
                    .format("%m-%d %H:%M 重置")
                    .to_string()
            })
            .unwrap_or_else(|| "重置时间未知".to_owned());
        format!("{}（剩 {:.0}%，{reset}）", self.name, self.remaining)
    }
}

/// An account cooling down until `until` (Unix seconds).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cooldown {
    pub until: i64,
    pub reason: String,
}

/// Where the account pool and its quota come from.
pub trait AccountSource: Send + Sync {
    /// Every account of the pool with its live quota.
    fn accounts(&self) -> BoxFut<'_, anyhow::Result<Vec<AccountInfo>>>;
}

/// The next account for a step: usable, paid, with quota left and not cooling down, the one
/// whose quota resets soonest first (then the one with most quota left).
pub fn choose<'a>(
    accounts: &'a [AccountInfo],
    cooling: &BTreeMap<String, Cooldown>,
    now: i64,
    except: Option<&str>,
) -> Option<&'a AccountInfo> {
    let mut candidates: Vec<&AccountInfo> = accounts
        .iter()
        .filter(|account| account.usable && account.plan != "free")
        .filter(|account| account.remaining >= MIN_REMAINING)
        .filter(|account| except != Some(account.name.as_str()))
        .filter(|account| {
            cooling
                .get(&account.name)
                .is_none_or(|cooldown| cooldown.until <= now)
        })
        .collect();
    candidates.sort_by(|a, b| {
        a.resets_at
            .unwrap_or(i64::MAX)
            .cmp(&b.resets_at.unwrap_or(i64::MAX))
            .then(b.remaining.total_cmp(&a.remaining))
    });
    candidates.first().copied()
}

/// The pool as the account status helper reports it (`codex-account-status --json`, the
/// program behind `codex-account-next list`), asked through this daemon's own binary.
pub struct StatusHelper {
    bin: PathBuf,
    home: PathBuf,
    codex: PathBuf,
}

impl StatusHelper {
    /// The helper for `codex_home`: `CODEXT_SUPERAGENT_ACCOUNT_STATUS_BIN`, else
    /// `~/.local/bin/codex-account-status`; `None` when it is not installed.
    pub fn for_home(codex_home: &Path) -> Option<Self> {
        let bin = std::env::var_os("CODEXT_SUPERAGENT_ACCOUNT_STATUS_BIN")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|home| PathBuf::from(home).join(".local/bin/codex-account-status"))
            })
            .filter(|bin| bin.is_file())?;
        let codex = std::env::current_exe().ok()?;
        Some(Self {
            bin,
            home: codex_home.to_owned(),
            codex,
        })
    }
}

/// Reads the helper's `--json` report.
pub fn parse_pool(report: &Value) -> Vec<AccountInfo> {
    report
        .get("accounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|account| {
            let name = account.get("account")?.as_str()?.to_owned();
            let text = |key: &str| account.get(key).and_then(Value::as_str).unwrap_or_default();
            let windows: Vec<&Value> = account
                .get("quota")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .collect();
            let remaining = windows
                .iter()
                .filter_map(|window| window.get("remaining_percent").and_then(Value::as_f64))
                .reduce(f64::min);
            let resets_at = windows
                .iter()
                .filter_map(|window| window.get("resets_at").and_then(Value::as_i64))
                .min();
            Some(AccountInfo {
                usable: text("status") == "live"
                    && text("auth_status") == "ready"
                    && account.get("error").is_none_or(Value::is_null)
                    && remaining.is_some(),
                name,
                plan: text("plan").to_owned(),
                remaining: remaining.unwrap_or(0.0),
                resets_at,
            })
        })
        .collect()
}

impl AccountSource for StatusHelper {
    fn accounts(&self) -> BoxFut<'_, anyhow::Result<Vec<AccountInfo>>> {
        Box::pin(async move {
            let run = tokio::process::Command::new(&self.bin)
                .arg("--home")
                .arg(&self.home)
                .arg("--binary")
                .arg(&self.codex)
                .args(["--json", "--timeout", "30"])
                .kill_on_drop(true)
                .output();
            let output = tokio::time::timeout(HELPER_TIMEOUT, run)
                .await
                .map_err(|_| anyhow::anyhow!("账号状态 {} 超时", self.bin.display()))??;
            anyhow::ensure!(
                output.status.success(),
                "账号状态 {} 失败：{}",
                self.bin.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
            let report: Value = serde_json::from_slice(&output.stdout)?;
            Ok(parse_pool(&report))
        })
    }
}
