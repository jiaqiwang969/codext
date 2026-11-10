//! Account switching for driven sessions and the Router. When a step's turn fails because its
//! account ran out of quota (`usageLimitExceeded`) or the model is at capacity on it
//! (`serverOverloaded`, "Selected model is at capacity"), that account cools down and the step goes
//! again on another account, chosen the way `codex-account-next list` orders the pool: the account
//! whose quota resets soonest first, since quota left at a reset is lost. Free accounts, accounts
//! with too little quota left and accounts still cooling down are skipped.
//!
//! Before that happens, `plan` decides from the pool alone whether a thread should leave its
//! account between turns: below the soft threshold of quota left it moves when that costs nothing,
//! below the hard threshold (or cooling down, or not usable) its account counts as spent.

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
/// Below this quota (percent) a thread leaves its account between turns.
pub const SOFT_REMAINING: f64 = 10.0;
/// Below this quota (percent) an account counts as spent, like one cooling down.
pub const HARD_REMAINING: f64 = 3.0;
/// How long an account cools down after the model was at capacity on it.
pub const CAPACITY_COOLDOWN: Duration = Duration::from_secs(15 * 60);
/// How long an out-of-quota account cools down when its reset time is unknown.
pub const QUOTA_COOLDOWN: Duration = Duration::from_secs(60 * 60);
const HELPER_TIMEOUT: Duration = Duration::from_secs(150);
/// How long one account's own read may take, through the daemon.
const ACCOUNT_TIMEOUT: Duration = Duration::from_secs(30);

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

    /// The incident kind (`incidents.rs`) of the trouble.
    pub fn kind(self) -> &'static str {
        match self {
            Self::UsageLimit => "usage_limit",
            Self::Capacity => "capacity",
        }
    }

    /// The trouble an incident kind reports; a rate limit is not one (it passes by itself).
    pub fn from_kind(kind: &str) -> Option<Self> {
        match kind {
            "usage_limit" => Some(Self::UsageLimit),
            "capacity" => Some(Self::Capacity),
            _ => None,
        }
    }
}

/// Now, in Unix seconds, as the cooldowns and the pool's reset times count.
pub(super) fn now() -> i64 {
    chrono::Utc::now().timestamp()
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

    /// One account's live quota, read on its own: far cheaper than the whole pool, so the
    /// checks read only the account a thread runs on and the pool only when it should move.
    /// The default reads the whole pool and picks the account.
    fn account<'a>(&'a self, name: &'a str) -> BoxFut<'a, anyhow::Result<AccountInfo>> {
        Box::pin(async move {
            self.accounts()
                .await?
                .into_iter()
                .find(|account| account.name == name)
                .ok_or_else(|| anyhow::anyhow!("账号池里没有 {name}"))
        })
    }
}

/// The next account for a step: usable, paid, with quota left and not cooling down, the one
/// whose quota resets soonest first (then the one with most quota left).
pub fn choose<'a>(
    accounts: &'a [AccountInfo],
    cooling: &BTreeMap<String, Cooldown>,
    now: i64,
    except: Option<&str>,
) -> Option<&'a AccountInfo> {
    choose_above(accounts, cooling, now, except, MIN_REMAINING)
}

/// Like `choose`, among the accounts with at least `floor` percent left (never below
/// `MIN_REMAINING`).
pub fn choose_above<'a>(
    accounts: &'a [AccountInfo],
    cooling: &BTreeMap<String, Cooldown>,
    now: i64,
    except: Option<&str>,
    floor: f64,
) -> Option<&'a AccountInfo> {
    let floor = floor.max(MIN_REMAINING);
    let mut candidates: Vec<&AccountInfo> = accounts
        .iter()
        .filter(|account| account.usable && account.plan != "free")
        .filter(|account| account.remaining >= floor)
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

/// The account the engine moves a thread to by itself: by `choose`'s order, one with quota at or
/// above the soft threshold, so the thread is not moved off it again at the next check; only when
/// there is none, one that is not spent (at or above the hard threshold, and `MIN_REMAINING`).
pub fn pick<'a>(
    accounts: &'a [AccountInfo],
    cooling: &BTreeMap<String, Cooldown>,
    now: i64,
    except: Option<&str>,
    thresholds: Thresholds,
) -> Option<&'a AccountInfo> {
    choose_above(accounts, cooling, now, except, thresholds.soft)
        .or_else(|| choose_above(accounts, cooling, now, except, thresholds.hard))
}

/// The quota left (percent) below which a thread leaves its account between turns (`soft`), and
/// below which the account counts as spent (`hard`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thresholds {
    pub soft: f64,
    pub hard: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            soft: SOFT_REMAINING,
            hard: HARD_REMAINING,
        }
    }
}

impl Thresholds {
    /// Keeps `hard` at or below `soft`.
    pub fn new(soft: f64, hard: f64) -> Self {
        Self {
            soft,
            hard: hard.min(soft),
        }
    }
}

/// How much a thread's account has left for its next turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Headroom {
    /// At or above the soft threshold.
    Enough,
    /// Below the soft threshold: move when no turn runs.
    Low,
    /// Below the hard threshold, cooling down or not usable.
    Spent,
}

/// Whether a turn runs on the thread now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    Idle,
    InTurn,
}

/// What to do about a thread's account.
#[derive(Clone, Debug, PartialEq)]
pub enum Plan {
    /// The account has headroom, or the pool does not know it.
    Stay,
    /// Move to `to` now.
    Move { to: AccountInfo, headroom: Headroom },
    /// A turn runs: move when it ends (to `to`, if it is still the best then). A running turn is
    /// never stopped for quota alone.
    AtTurnEnd { to: AccountInfo, headroom: Headroom },
    /// No other account can take over: stay.
    NoneFree(Headroom),
}

/// How much `current` has left for the next turn; `None` when the pool does not know it.
pub fn headroom(
    accounts: &[AccountInfo],
    cooling: &BTreeMap<String, Cooldown>,
    now: i64,
    current: &str,
    thresholds: Thresholds,
) -> Option<Headroom> {
    let account = accounts.iter().find(|account| account.name == current)?;
    let cooling = cooling
        .get(&account.name)
        .is_some_and(|cooldown| cooldown.until > now);
    Some(
        if cooling || !account.usable || account.remaining < thresholds.hard {
            Headroom::Spent
        } else if account.remaining < thresholds.soft {
            Headroom::Low
        } else {
            Headroom::Enough
        },
    )
}

/// Whether a thread on `current` should move to another account, and to which. A low account is
/// left only for one above the soft threshold; a spent one for any account `pick` takes.
pub fn plan(
    accounts: &[AccountInfo],
    cooling: &BTreeMap<String, Cooldown>,
    now: i64,
    current: &str,
    thresholds: Thresholds,
    activity: Activity,
) -> Plan {
    let (headroom, to) = match headroom(accounts, cooling, now, current, thresholds) {
        None | Some(Headroom::Enough) => return Plan::Stay,
        Some(Headroom::Low) => (
            Headroom::Low,
            choose_above(accounts, cooling, now, Some(current), thresholds.soft),
        ),
        Some(Headroom::Spent) => (
            Headroom::Spent,
            pick(accounts, cooling, now, Some(current), thresholds),
        ),
    };
    match (to.cloned(), activity) {
        (None, Activity::Idle | Activity::InTurn) => Plan::NoneFree(headroom),
        (Some(to), Activity::Idle) => Plan::Move { to, headroom },
        (Some(to), Activity::InTurn) => Plan::AtTurnEnd { to, headroom },
    }
}

/// Why `current` should be left, in the person's words: `剩 8%`, `冷却中（模型满载）`.
pub fn shortfall(
    accounts: &[AccountInfo],
    cooling: &BTreeMap<String, Cooldown>,
    now: i64,
    current: &str,
) -> String {
    if let Some(cooldown) = cooling.get(current).filter(|cooldown| cooldown.until > now) {
        return format!("冷却中（{}）", cooldown.reason);
    }
    match accounts.iter().find(|account| account.name == current) {
        Some(account) if !account.usable => "暂时用不了".to_owned(),
        Some(account) => format!("剩 {:.0}%", account.remaining),
        None => "额度未知".to_owned(),
    }
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

/// One account as the daemon reports it (`codex app-server daemon account-status <name>`), its
/// quota read the way `codex-account-status` reads it: the least quota left and the soonest reset
/// over its windows. It is usable when the read has a window and the daemon does not deny it
/// ordinary use.
pub fn parse_account(name: &str, report: &Value) -> AccountInfo {
    let limits = report.get("limits").unwrap_or(&Value::Null);
    let mut buckets: Vec<(String, &Value)> = limits
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(id, bucket)| (id.clone(), bucket))
        .collect();
    if buckets.is_empty()
        && let Some(bucket) = limits.get("rateLimits").filter(|bucket| bucket.is_object())
    {
        buckets.push((String::new(), bucket));
    }
    let mut plan = None;
    let mut remaining: Option<f64> = None;
    let mut resets_at: Option<i64> = None;
    let mut window = |left: f64, reset: Option<i64>| {
        let left = left.clamp(0.0, 100.0);
        remaining = Some(remaining.map_or(left, |least| least.min(left)));
        if let Some(reset) = reset {
            resets_at = Some(resets_at.map_or(reset, |soonest| soonest.min(reset)));
        }
    };
    for (key, bucket) in buckets {
        let id = bucket
            .get("limitId")
            .and_then(Value::as_str)
            .unwrap_or(&key);
        // Not an account quota; `codex-account-status` leaves it out too.
        if id.eq_ignore_ascii_case("bengalfox") {
            continue;
        }
        if plan.is_none() {
            plan = bucket
                .get("planType")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        for kind in ["primary", "secondary"] {
            let Some(limit) = bucket.get(kind).filter(|limit| limit.is_object()) else {
                continue;
            };
            if let Some(used) = limit.get("usedPercent").and_then(Value::as_f64) {
                window(100.0 - used, limit.get("resetsAt").and_then(Value::as_i64));
            }
        }
        if let Some(individual) = bucket
            .get("individualLimit")
            .filter(|limit| limit.is_object())
            && let Some(left) = individual.get("remainingPercent").and_then(Value::as_f64)
        {
            window(left, individual.get("resetsAt").and_then(Value::as_i64));
        }
    }
    let allowed = limits.get("ordinaryUsageAllowed").and_then(Value::as_bool) != Some(false);
    AccountInfo {
        name: name.to_owned(),
        plan: plan.unwrap_or_default(),
        usable: remaining.is_some() && allowed,
        remaining: remaining.unwrap_or(0.0),
        resets_at,
    }
}

impl AccountSource for StatusHelper {
    /// Through the daemon's own credential managers, as `codex-account-status` reads each
    /// account, so a read never refreshes credentials beside the daemon.
    fn account<'a>(&'a self, name: &'a str) -> BoxFut<'a, anyhow::Result<AccountInfo>> {
        Box::pin(async move {
            let timeout_ms = ACCOUNT_TIMEOUT
                .saturating_sub(Duration::from_secs(5))
                .as_millis();
            let mut command = tokio::process::Command::new(&self.codex);
            command
                .args(["app-server", "daemon", "account-status", name])
                .arg("--timeout-ms")
                .arg(timeout_ms.to_string())
                .env("CODEX_HOME", &self.home)
                .env("CODEX_SQLITE_HOME", &self.home)
                .env("RUST_LOG", "error")
                .kill_on_drop(true);
            // The daemon chooses the credentials; none may come from this process.
            for inherited in [
                "CODEX_AUTH_PROFILE",
                "CODEX_ACCOUNT_PROFILE",
                "CODEX_ACCESS_TOKEN",
                "CODEX_API_KEY",
                "OPENAI_API_KEY",
            ] {
                command.env_remove(inherited);
            }
            let output = tokio::time::timeout(ACCOUNT_TIMEOUT, command.output())
                .await
                .map_err(|_| anyhow::anyhow!("读账号 {name} 的额度超时"))??;
            anyhow::ensure!(
                output.status.success(),
                "读账号 {name} 的额度失败：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            let report: Value = serde_json::from_slice(&output.stdout)?;
            Ok(parse_account(name, &report))
        })
    }

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
