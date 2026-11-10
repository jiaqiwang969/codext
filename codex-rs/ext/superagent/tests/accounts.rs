//! Keeping the Router and the driven sessions on accounts with quota left, before they run out:
//! the decision from the pool, the watch, the look when a Router turn ends (holding its queued
//! input for the move), the gate before the engine hands a Router anything, the check before each
//! step of a drive, a drive started on a named account, and the safety net under all of it: the
//! move after a Router turn its account failed or kept retrying. A spent account the person
//! refilled is free again as soon as a read shows its quota back.

mod common;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_protocol::ThreadId;
use codex_superagent_extension::BoxFut;
use codex_superagent_extension::Effects;
use codex_superagent_extension::HealthBoard;
use codex_superagent_extension::Store;
use codex_superagent_extension::drive::AccountInfo;
use codex_superagent_extension::drive::AccountMove;
use codex_superagent_extension::drive::AccountPolicy;
use codex_superagent_extension::drive::AccountSource;
use codex_superagent_extension::drive::Activity;
use codex_superagent_extension::drive::Audit;
use codex_superagent_extension::drive::COOLDOWN_KEY;
use codex_superagent_extension::drive::Cooldown;
use codex_superagent_extension::drive::DriveMode;
use codex_superagent_extension::drive::DriveOptions;
use codex_superagent_extension::drive::DriveService;
use codex_superagent_extension::drive::DriveStart;
use codex_superagent_extension::drive::DriveState;
use codex_superagent_extension::drive::EndedCooldown;
use codex_superagent_extension::drive::Headroom;
use codex_superagent_extension::drive::Judge;
use codex_superagent_extension::drive::KIND_RESCUE;
use codex_superagent_extension::drive::KIND_ROUTER;
use codex_superagent_extension::drive::KIND_WATCH;
use codex_superagent_extension::drive::Plan;
use codex_superagent_extension::drive::Say;
use codex_superagent_extension::drive::Thresholds;
use codex_superagent_extension::drive::Trouble;
use codex_superagent_extension::drive::choose;
use codex_superagent_extension::drive::parse_account;
use codex_superagent_extension::drive::pick;
use codex_superagent_extension::drive::plan;
use codex_superagent_extension::drive::rescue_due;
use codex_superagent_extension::host::DeliveryLedger;
use codex_superagent_extension::host::DeliveryMode;
use codex_superagent_extension::host::GoalStatus;
use codex_superagent_extension::host::LedgerHost;
use codex_superagent_extension::host::StepStart;
use codex_superagent_extension::host::SuperagentHost;
use codex_superagent_extension::host::ThreadSpec;
use codex_superagent_extension::host::TurnBrief;
use codex_superagent_extension::incidents;
use codex_superagent_extension::incidents::Incident;
use common::FakeClock;
use common::T0;
use common::open_store;
use common::wait_until_async;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

const ROUTER: &str = "019a0000-0000-7000-8000-00000000e0aa";
const SESSION: &str = "019a0000-0000-7000-8000-00000000e001";
const OPENED: &str = "019a0000-0000-7000-8000-00000000e002";

fn lock<T>(cell: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    cell.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A host that keeps each thread's account and latest turns, and logs what the engine asks of
/// it in order. Like the real host, it refuses to move a thread whose turn runs and does nothing
/// for an account the thread already has; for a thread that is not loaded it answers the account
/// the thread will load with, and binding one keeps it unloaded.
#[derive(Default)]
struct Host {
    log: Mutex<Vec<String>>,
    profiles: Mutex<HashMap<String, String>>,
    /// The threads that are not loaded.
    cold: Mutex<HashSet<String>>,
    /// Each thread's latest turns, newest first.
    turns: Mutex<HashMap<String, Vec<TurnBrief>>>,
    delivered: Mutex<Vec<String>>,
    opened: Mutex<Vec<(PathBuf, Option<String>)>>,
}

impl Host {
    fn log(&self) -> Vec<String> {
        lock(&self.log).clone()
    }
    fn on(&self, thread: &str, account: &str) {
        lock(&self.profiles).insert(thread.to_owned(), account.to_owned());
    }
    fn account(&self, thread: &str) -> Option<String> {
        lock(&self.profiles).get(thread).cloned()
    }
    fn turns(&self, thread: &str, turns: Vec<TurnBrief>) {
        lock(&self.turns).insert(thread.to_owned(), turns);
    }
    fn delivered(&self) -> Vec<String> {
        lock(&self.delivered).clone()
    }
    fn unload(&self, thread: &str) {
        lock(&self.cold).insert(thread.to_owned());
    }
    fn loaded(&self, thread: &str) -> bool {
        !lock(&self.cold).contains(thread)
    }
}

impl SuperagentHost for Host {
    fn start_thread(&self, spec: ThreadSpec) -> BoxFut<'_, anyhow::Result<ThreadId>> {
        if let Some(profile) = &spec.profile {
            self.on(OPENED, profile);
        }
        lock(&self.opened).push((spec.cwd, spec.profile));
        Box::pin(async { Ok(ThreadId::from_string(OPENED)?) })
    }
    fn thread_profile(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<Option<String>>> {
        let account = self.account(&thread.to_string());
        Box::pin(async move { Ok(account) })
    }
    fn bind_profile(&self, thread: ThreadId, profile: String) -> BoxFut<'_, anyhow::Result<()>> {
        let thread = thread.to_string();
        let running = lock(&self.turns)
            .get(&thread)
            .and_then(|turns| turns.first())
            .is_some_and(|turn| turn.status == "inProgress");
        let result = if running {
            Err(anyhow::anyhow!(
                "wait for the current task to finish before switching accounts"
            ))
        } else {
            if self.account(&thread).as_deref() != Some(profile.as_str()) {
                lock(&self.log).push(format!("bind:{thread}:{profile}"));
                self.on(&thread, &profile);
            }
            Ok(())
        };
        Box::pin(async move { result })
    }
    fn resume_thread(
        &self,
        thread: ThreadId,
        _role: Option<String>,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        let thread = thread.to_string();
        if lock(&self.cold).remove(&thread) {
            lock(&self.log).push(format!("load:{thread}"));
        }
        Box::pin(async { Ok(()) })
    }
    fn deliver(
        &self,
        thread: ThreadId,
        text: String,
        mode: DeliveryMode,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        lock(&self.log).push(format!("deliver:{thread}:{mode:?}"));
        lock(&self.delivered).push(text);
        Box::pin(async { Ok(()) })
    }
    fn start_step(&self, thread: ThreadId, text: String) -> BoxFut<'_, anyhow::Result<StepStart>> {
        lock(&self.log).push(format!("step:{thread}"));
        lock(&self.delivered).push(text);
        Box::pin(async { Ok(StepStart::Started { turn: None }) })
    }
    fn session_turns(
        &self,
        thread: ThreadId,
        limit: u32,
    ) -> BoxFut<'_, anyhow::Result<Vec<TurnBrief>>> {
        let turns: Vec<TurnBrief> = lock(&self.turns)
            .get(&thread.to_string())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .take(limit as usize)
            .collect();
        Box::pin(async move { Ok(turns) })
    }
    fn start_queued(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<bool>> {
        lock(&self.log).push(format!("start_queued:{thread}"));
        Box::pin(async { Ok(false) })
    }
    fn interrupt(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        let thread = thread.to_string();
        lock(&self.log).push(format!("interrupt:{thread}"));
        if let Some(turn) = lock(&self.turns)
            .get_mut(&thread)
            .and_then(|turns| turns.first_mut())
            .filter(|turn| turn.status == "inProgress")
        {
            turn.status = "interrupted".to_owned();
        }
        Box::pin(async { Ok(()) })
    }
    fn clean_background(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn background_commands(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<String>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn children(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<ThreadId>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn goal_set(
        &self,
        _thread: ThreadId,
        _objective: String,
        _token_budget: Option<i64>,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn goal_clear(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn goal_status(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Option<GoalStatus>>> {
        Box::pin(async { Ok(None) })
    }
}

/// The pool the engine reads: the whole of it (counted), or one account on its own (logged).
#[derive(Default)]
struct Pool {
    accounts: Mutex<Vec<AccountInfo>>,
    full: AtomicUsize,
    own: Mutex<Vec<String>>,
}

impl Pool {
    fn set(&self, accounts: Vec<AccountInfo>) {
        *lock(&self.accounts) = accounts;
    }
    fn full_reads(&self) -> usize {
        self.full.load(Ordering::SeqCst)
    }
    fn own_reads(&self) -> Vec<String> {
        lock(&self.own).clone()
    }
}

impl AccountSource for Pool {
    fn accounts(&self) -> BoxFut<'_, anyhow::Result<Vec<AccountInfo>>> {
        self.full.fetch_add(1, Ordering::SeqCst);
        let accounts = lock(&self.accounts).clone();
        Box::pin(async move { Ok(accounts) })
    }
    fn account<'a>(&'a self, name: &'a str) -> BoxFut<'a, anyhow::Result<AccountInfo>> {
        lock(&self.own).push(name.to_owned());
        let found = lock(&self.accounts)
            .iter()
            .find(|account| account.name == name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no account {name}"));
        Box::pin(async move { found })
    }
}

struct NoJudge;

impl Judge for NoJudge {
    fn audit<'a>(&'a self, _project_home: &'a Path) -> BoxFut<'a, anyhow::Result<Audit>> {
        Box::pin(async { Ok(Audit::default()) })
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// An account of the pool whose quota resets `resets_in_hours` from now, at `HH:00`.
fn account(name: &str, plan: &str, remaining: f64, resets_in_hours: i64) -> AccountInfo {
    AccountInfo {
        name: name.to_owned(),
        plan: plan.to_owned(),
        usable: true,
        remaining,
        resets_at: Some((now() / 3600 + resets_in_hours) * 3600),
    }
}

fn turn(id: &str, status: &str, words: &[&str]) -> TurnBrief {
    TurnBrief {
        id: id.to_owned(),
        status: status.to_owned(),
        user: words.iter().map(|words| (*words).to_owned()).collect(),
        ..TurnBrief::default()
    }
}

fn failed(id: &str, kind: &str, message: &str, words: &[&str]) -> TurnBrief {
    TurnBrief {
        error: Some(message.to_owned()),
        error_kind: Some(kind.to_owned()),
        ..turn(id, "failed", words)
    }
}

/// The moves the drive kept, without their times.
async fn moves(drive: &DriveService) -> anyhow::Result<Vec<AccountMove>> {
    Ok(drive
        .account_moves()
        .await?
        .into_iter()
        .map(|kept| AccountMove {
            at: String::new(),
            ..kept
        })
        .collect())
}

fn moved(thread: &str, from: &str, to: &str, why: &str) -> AccountMove {
    AccountMove {
        at: String::new(),
        thread_id: thread.to_owned(),
        from: Some(from.to_owned()),
        to: to.to_owned(),
        why: why.to_owned(),
    }
}

/// The cooldowns a read ended early, without their times.
async fn ended(drive: &DriveService) -> anyhow::Result<Vec<EndedCooldown>> {
    Ok(drive
        .ended_cooldowns()
        .await?
        .into_iter()
        .map(|kept| EndedCooldown {
            at: String::new(),
            ..kept
        })
        .collect())
}

fn ended_early(account: &str, why: &str) -> EndedCooldown {
    EndedCooldown {
        at: String::new(),
        account: account.to_owned(),
        why: why.to_owned(),
    }
}

/// The cooldown of an account whose quota ran out while it showed `remaining` percent left.
fn out_of_quota(until: i64, remaining: f64) -> Cooldown {
    Cooldown {
        until,
        reason: "账号额度用完".to_owned(),
        kind: Some(Trouble::UsageLimit),
        remaining: Some(remaining),
    }
}

/// Writes the cooldown ledger as given: typed rows, or rows as earlier versions wrote them.
async fn seed_cooldowns(s: &Setup, rows: &impl serde::Serialize) -> anyhow::Result<()> {
    s.store
        .atomic(async |tx| tx.kv_set(COOLDOWN_KEY, rows).await)
        .await
}

struct Setup {
    _dir: TempDir,
    work: TempDir,
    host: Arc<Host>,
    pool: Arc<Pool>,
    effects: Effects,
    store: Store,
    drive: DriveService,
}

/// The effect queue is not started: each test runs the effects' work itself, in order.
async fn setup_with(policy: AccountPolicy) -> anyhow::Result<Setup> {
    let (dir, store) = open_store().await?;
    let host = Arc::new(Host::default());
    let pool = Arc::new(Pool::default());
    let effects =
        Effects::new(store.clone(), HealthBoard::new(), FakeClock::at(T0).clock()).await?;
    let drive = DriveService::install_with(
        store.clone(),
        effects.clone(),
        host.clone() as Arc<dyn SuperagentHost>,
        Arc::new(NoJudge),
        DriveOptions {
            accounts: Some(pool.clone() as Arc<dyn AccountSource>),
            stall: Duration::from_secs(3600),
            policy,
            routers: Arc::new(|| vec![ROUTER.to_owned()]),
            retry_after: Duration::ZERO,
        },
    )
    .await?;
    Ok(Setup {
        _dir: dir,
        work: TempDir::new()?,
        host,
        pool,
        effects,
        store,
        drive,
    })
}

async fn setup() -> anyhow::Result<Setup> {
    setup_with(AccountPolicy::default()).await
}

fn simple(folder: &Path, account: Option<&str>) -> DriveStart {
    DriveStart {
        project_home: folder.to_owned(),
        title: "整理笔记".to_owned(),
        mode: DriveMode::Simple,
        max_steps: None,
        request: "整理笔记目录，按主题分好".to_owned(),
        goal: None,
        first: "把笔记按主题分进子目录，做完告诉我怎么检查。".to_owned(),
        account: account.map(str::to_owned),
        router: None,
    }
}

/// A case of the plan: its name, the pool, the cooldowns, the current account, the activity.
type Case<'a> = (
    &'a str,
    Vec<AccountInfo>,
    &'a BTreeMap<String, Cooldown>,
    &'a str,
    Activity,
);

#[test]
fn the_plan_leaves_a_low_or_spent_account_only_for_one_that_holds() {
    let thresholds = Thresholds::default();
    let now = now();
    let none = BTreeMap::new();
    let low = account("router", "pro", 8.0, 2);
    let spent = account("router", "pro", 2.0, 2);
    let near_empty = account("near-empty", "pro", 7.0, 1);
    let plenty = account("plenty", "pro", 70.0, 30);
    let sooner = account("sooner", "pro", 40.0, 10);
    let free = account("free", "free", 100.0, 1);
    let unusable = AccountInfo {
        usable: false,
        ..account("router", "pro", 90.0, 2)
    };
    let cooling = BTreeMap::from([(
        "router".to_owned(),
        Cooldown {
            until: now + 600,
            reason: "模型满载".to_owned(),
            kind: Some(Trouble::Capacity),
            remaining: None,
        },
    )]);
    let cases: Vec<Case<'_>> = vec![
        (
            "enough",
            vec![plenty.clone(), sooner.clone()],
            &none,
            "plenty",
            Activity::Idle,
        ),
        (
            "unknown to the pool",
            vec![plenty.clone()],
            &none,
            "elsewhere",
            Activity::Idle,
        ),
        (
            "low between turns",
            vec![
                low.clone(),
                near_empty.clone(),
                plenty.clone(),
                free.clone(),
            ],
            &none,
            "router",
            Activity::Idle,
        ),
        (
            "low in a turn",
            vec![low.clone(), plenty.clone()],
            &none,
            "router",
            Activity::InTurn,
        ),
        (
            "low with only low accounts to go to",
            vec![low.clone(), near_empty.clone(), free.clone()],
            &none,
            "router",
            Activity::Idle,
        ),
        (
            "spent with only a low account to go to",
            vec![spent.clone(), near_empty.clone()],
            &none,
            "router",
            Activity::Idle,
        ),
        (
            "spent in a turn, soonest reset first",
            vec![spent.clone(), plenty.clone(), sooner.clone()],
            &none,
            "router",
            Activity::InTurn,
        ),
        (
            "cooling down",
            vec![account("router", "pro", 90.0, 2), plenty.clone()],
            &cooling,
            "router",
            Activity::Idle,
        ),
        (
            "not usable",
            vec![unusable, plenty.clone()],
            &none,
            "router",
            Activity::Idle,
        ),
        (
            "spent and nothing free",
            vec![spent, free.clone()],
            &none,
            "router",
            Activity::Idle,
        ),
    ];
    let planned: Vec<(&str, Plan)> = cases
        .into_iter()
        .map(|(case, pool, cooling, current, activity)| {
            (
                case,
                plan(&pool, cooling, now, current, thresholds, activity),
            )
        })
        .collect();
    assert_eq!(
        planned,
        vec![
            ("enough", Plan::Stay),
            ("unknown to the pool", Plan::Stay),
            (
                "low between turns",
                Plan::Move {
                    to: plenty.clone(),
                    headroom: Headroom::Low
                }
            ),
            (
                "low in a turn",
                Plan::AtTurnEnd {
                    to: plenty.clone(),
                    headroom: Headroom::Low
                }
            ),
            (
                "low with only low accounts to go to",
                Plan::NoneFree(Headroom::Low)
            ),
            (
                "spent with only a low account to go to",
                Plan::Move {
                    to: near_empty.clone(),
                    headroom: Headroom::Spent
                }
            ),
            (
                "spent in a turn, soonest reset first",
                Plan::AtTurnEnd {
                    to: sooner,
                    headroom: Headroom::Spent
                }
            ),
            (
                "cooling down",
                Plan::Move {
                    to: plenty.clone(),
                    headroom: Headroom::Spent
                }
            ),
            (
                "not usable",
                Plan::Move {
                    to: plenty.clone(),
                    headroom: Headroom::Spent
                }
            ),
            ("spent and nothing free", Plan::NoneFree(Headroom::Spent)),
        ]
    );

    // `choose` keeps its rules (soonest reset among paid accounts with 5% left); `pick`, which
    // the engine moves by, skips an account it would have to leave again at the next look.
    let pool = vec![low, near_empty, plenty, free];
    let names = |chosen: Option<&AccountInfo>| chosen.map(|account| account.name.clone());
    assert_eq!(
        (
            names(choose(&pool, &none, now, Some("router"))),
            names(pick(&pool, &none, now, Some("router"), thresholds)),
        ),
        (Some("near-empty".to_owned()), Some("plenty".to_owned()))
    );
}

#[test]
fn the_knobs_come_from_the_environment_and_fall_back_to_the_defaults() {
    assert_eq!(
        AccountPolicy::default(),
        AccountPolicy {
            thresholds: Thresholds {
                soft: 10.0,
                hard: 3.0
            },
            watch_every: Duration::from_secs(120),
            rescue_after: 2,
            rescue_window: Duration::from_secs(180),
            max_per_account: 4,
        }
    );
    assert_eq!(AccountPolicy::from_env(|_| None), AccountPolicy::default());
    let vars = HashMap::from([
        ("CODEXT_SUPERAGENT_QUOTA_SOFT_PERCENT", "15"),
        // above the soft threshold: kept at it
        ("CODEXT_SUPERAGENT_QUOTA_HARD_PERCENT", "20"),
        ("CODEXT_SUPERAGENT_QUOTA_WATCH_SECS", "0"),
        ("CODEXT_SUPERAGENT_ROUTER_RESCUE_THRESHOLD", " 3 "),
        ("CODEXT_SUPERAGENT_ROUTER_RESCUE_WINDOW_SECS", "soon"),
        ("CODEXT_SUPERAGENT_MAX_PER_ACCOUNT", "3"),
    ]);
    assert_eq!(
        AccountPolicy::from_env(|name| vars.get(name).map(|value| (*value).to_owned())),
        AccountPolicy {
            thresholds: Thresholds {
                soft: 15.0,
                hard: 15.0
            },
            watch_every: Duration::ZERO,
            rescue_after: 3,
            rescue_window: Duration::from_secs(180),
            max_per_account: 3,
        }
    );
    let out_of_range = HashMap::from([("CODEXT_SUPERAGENT_QUOTA_SOFT_PERCENT", "150")]);
    assert_eq!(
        AccountPolicy::from_env(|name| out_of_range.get(name).map(|value| (*value).to_owned())),
        AccountPolicy::default()
    );
}

#[tokio::test]
async fn the_watch_reads_a_routers_own_account_and_the_pool_only_to_move_it() -> anyhow::Result<()>
{
    let s = setup().await?;
    s.drive.start_account_watch().await?;
    assert_eq!(
        s.effects.queued(KIND_WATCH).await?,
        vec![("pool".to_owned(), json!({}))]
    );
    s.host.on(ROUTER, "router-acct");
    s.host
        .turns(ROUTER, vec![turn("t1", "completed", &["看看进展"])]);
    s.pool.set(vec![
        account("router-acct", "pro", 60.0, 2),
        account("next", "pro", 60.0, 5),
        account("later", "pro", 90.0, 40),
    ]);
    // A healthy account: its own read and nothing more.
    s.drive.watch_accounts().await;
    assert_eq!(
        (s.pool.full_reads(), s.host.log()),
        (0, Vec::<String>::new())
    );

    // It runs low: the pool is read once, and the idle Router moves at once.
    s.pool.set(vec![
        account("router-acct", "pro", 8.0, 2),
        account("next", "pro", 60.0, 5),
        account("later", "pro", 90.0, 40),
    ]);
    s.drive.watch_accounts().await;
    assert_eq!(s.host.log(), vec![format!("bind:{ROUTER}:next")]);
    assert_eq!(
        moves(&s.drive).await?,
        vec![moved(
            ROUTER,
            "router-acct",
            "next",
            "Router 原账号剩 8%，提前换号"
        )]
    );
    // The next look reads the new account only.
    s.drive.watch_accounts().await;

    // Still idle, the new account runs low too (other sessions share it): the pool as read a
    // moment ago, with each account as read since, finds the next one.
    s.pool.set(vec![
        account("router-acct", "pro", 8.0, 2),
        account("next", "pro", 4.0, 5),
        account("later", "pro", 90.0, 40),
    ]);
    s.drive.watch_accounts().await;
    // That one runs low as well, and none known has quota above the soft threshold: it stays,
    // and never goes back to an account found low since the pool was read.
    s.pool.set(vec![
        account("router-acct", "pro", 8.0, 2),
        account("next", "pro", 4.0, 5),
        account("later", "pro", 5.0, 40),
    ]);
    s.drive.watch_accounts().await;
    assert_eq!(
        s.host.log(),
        vec![
            format!("bind:{ROUTER}:next"),
            format!("bind:{ROUTER}:later")
        ]
    );
    assert_eq!(
        (s.pool.full_reads(), s.pool.own_reads()),
        (
            1,
            ["router-acct", "router-acct", "next", "next", "later"]
                .map(str::to_owned)
                .to_vec()
        )
    );
    Ok(())
}

#[tokio::test]
async fn a_router_never_leaves_an_account_twice_after_one_turn() -> anyhow::Result<()> {
    // Every look reads the pool again.
    let s = setup_with(AccountPolicy {
        watch_every: Duration::from_nanos(1),
        ..AccountPolicy::default()
    })
    .await?;
    s.host.on(ROUTER, "router-acct");
    s.host.turns(ROUTER, vec![turn("t1", "completed", &[])]);
    s.pool.set(vec![
        account("router-acct", "pro", 8.0, 2),
        account("next", "pro", 60.0, 5),
    ]);
    s.drive.watch_accounts().await;
    // The quota of the first account resets while the second runs low: it goes back once.
    s.pool.set(vec![
        account("router-acct", "pro", 95.0, 2),
        account("next", "pro", 4.0, 5),
    ]);
    s.drive.watch_accounts().await;
    // Low again before the Router's next turn: it stays rather than bounce.
    s.pool.set(vec![
        account("router-acct", "pro", 8.0, 2),
        account("next", "pro", 95.0, 5),
    ]);
    s.drive.watch_accounts().await;
    assert_eq!(
        s.host.log(),
        vec![
            format!("bind:{ROUTER}:next"),
            format!("bind:{ROUTER}:router-acct")
        ]
    );
    // After another turn it may leave it again.
    s.host.turns(ROUTER, vec![turn("t2", "completed", &[])]);
    s.drive.watch_accounts().await;
    assert_eq!(s.host.log().len(), 3);
    Ok(())
}

#[tokio::test]
async fn healthy_accounts_are_read_on_their_own_and_the_pool_is_never_read() -> anyhow::Result<()> {
    let s = setup().await?;
    s.host.on(ROUTER, "router-acct");
    s.host.on(SESSION, "session-acct");
    s.host.turns(ROUTER, vec![turn("t1", "completed", &[])]);
    s.pool.set(vec![
        account("router-acct", "pro", 60.0, 2),
        account("session-acct", "pro", 70.0, 3),
        account("other", "pro", 90.0, 4),
    ]);
    // The watch reads the Router's account.
    s.drive.watch_accounts().await;
    // A delivery right after uses the account as the watch read it.
    let gate = Arc::new(OnceLock::new());
    let _ = gate.set(s.drive.delivery_gate());
    let engine_host = LedgerHost::with_gate(
        s.host.clone() as Arc<dyn SuperagentHost>,
        Arc::new(DeliveryLedger::default()),
        gate,
    );
    engine_host
        .deliver(
            ThreadId::from_string(ROUTER)?,
            "[通知] 推进 · 整理笔记 · 交了结论".to_owned(),
            DeliveryMode::Queue,
        )
        .await?;
    // So does the end of a Router turn.
    s.drive.router_turn_started(ROUTER, "t2");
    s.host.turns(ROUTER, vec![turn("t2", "completed", &[])]);
    s.drive.router_turn_ended(ROUTER).await?;
    assert!(!s.drive.holds_queue(ROUTER));
    s.drive.router_turn_boundary(ROUTER, Some("t2")).await?;
    // A step reads its session's account.
    s.drive.start(SESSION, simple(s.work.path(), None)).await?;
    assert_eq!(
        (s.pool.full_reads(), s.pool.own_reads(), s.host.log()),
        (
            0,
            vec!["router-acct".to_owned(), "session-acct".to_owned()],
            vec![format!("deliver:{ROUTER}:Queue"), format!("step:{SESSION}")]
        )
    );
    Ok(())
}

#[tokio::test]
async fn an_unloaded_router_on_a_low_account_moves_by_its_binding_without_loading()
-> anyhow::Result<()> {
    let s = setup().await?;
    // After a restart the Router is not loaded; the host answers the account it will load with.
    s.host.on(ROUTER, "router-acct");
    s.host.unload(ROUTER);
    s.host
        .turns(ROUTER, vec![turn("t1", "interrupted", &["看看进展"])]);
    s.pool.set(vec![
        account("router-acct", "pro", 8.0, 2),
        account("next", "pro", 60.0, 5),
    ]);
    s.drive.watch_accounts().await;
    assert_eq!(s.host.log(), vec![format!("bind:{ROUTER}:next")]);
    assert_eq!(
        (s.host.loaded(ROUTER), s.host.account(ROUTER)),
        (false, Some("next".to_owned())),
        "the binding moved, the Router stays unloaded"
    );
    assert_eq!(
        moves(&s.drive).await?,
        vec![moved(
            ROUTER,
            "router-acct",
            "next",
            "Router 原账号剩 8%，提前换号"
        )]
    );
    Ok(())
}

#[test]
fn one_accounts_report_from_the_daemon_reads_as_its_quota() {
    let report = json!({
        "limits": {
            "ordinaryUsageAllowed": true,
            "rateLimits": { "limitId": "codex", "planType": "pro",
                "primary": { "usedPercent": 30, "windowDurationMins": 300, "resetsAt": 1793900000 } },
            "rateLimitsByLimitId": {
                "codex": { "limitId": "codex", "planType": "pro",
                    "primary": { "usedPercent": 30, "windowDurationMins": 300, "resetsAt": 1793900000 },
                    "secondary": { "usedPercent": 55, "windowDurationMins": 10080, "resetsAt": 1794400000 } },
                "bengalfox": { "limitId": "bengalfox",
                    "primary": { "usedPercent": 99, "resetsAt": 1793000000 } },
            },
        },
        "usage": {},
        "usage_error": null,
    });
    let denied = json!({ "limits": {
        "ordinaryUsageAllowed": false,
        "rateLimits": { "planType": "plus",
            "primary": { "usedPercent": 100, "resetsAt": 1793950000 },
            "individualLimit": { "limit": "1", "used": "1", "remainingPercent": 0, "resetsAt": 1793960000 } },
        "rateLimitsByLimitId": null,
    }});
    let unread = json!({ "limits": {} });
    assert_eq!(
        vec![
            parse_account("kalliesallerwu", &report),
            parse_account("JannLinegarA0k", &denied),
            parse_account("broken", &unread),
        ],
        vec![
            AccountInfo {
                name: "kalliesallerwu".to_owned(),
                plan: "pro".to_owned(),
                usable: true,
                remaining: 45.0,
                resets_at: Some(1793900000),
            },
            AccountInfo {
                name: "JannLinegarA0k".to_owned(),
                plan: "plus".to_owned(),
                usable: false,
                remaining: 0.0,
                resets_at: Some(1793950000),
            },
            AccountInfo {
                name: "broken".to_owned(),
                plan: String::new(),
                usable: false,
                remaining: 0.0,
                resets_at: None,
            },
        ]
    );
}

#[tokio::test]
async fn a_router_in_a_turn_moves_when_the_turn_ends_and_its_queue_waits_for_the_move()
-> anyhow::Result<()> {
    let s = setup().await?;
    s.host.on(ROUTER, "router-acct");
    s.pool.set(vec![
        account("router-acct", "pro", 2.0, 2),
        account("next", "pro", 60.0, 5),
    ]);
    s.drive.router_turn_started(ROUTER, "t2");
    s.host
        .turns(ROUTER, vec![turn("t2", "inProgress", &["帮我看看进展"])]);
    // Spent, but the turn runs: it is not stopped for quota.
    s.drive.watch_accounts().await;
    assert_eq!(s.host.log(), Vec::<String>::new());
    assert!(!s.drive.holds_queue(ROUTER));

    // The turn ends: the queue would start the next turn at once, so it waits for the move.
    s.drive.router_turn_ended(ROUTER).await?;
    assert!(s.drive.holds_queue(ROUTER));
    assert_eq!(
        s.effects.queued(KIND_ROUTER).await?,
        vec![(format!("{ROUTER}#t2"), json!({ "turn": "t2" }))]
    );
    s.host
        .turns(ROUTER, vec![turn("t2", "completed", &["帮我看看进展"])]);
    s.drive.router_turn_boundary(ROUTER, Some("t2")).await?;
    assert_eq!(
        s.host.log(),
        vec![
            format!("bind:{ROUTER}:next"),
            format!("start_queued:{ROUTER}")
        ]
    );
    assert!(!s.drive.holds_queue(ROUTER));
    assert_eq!(
        s.host.delivered(),
        Vec::<String>::new(),
        "nothing is handed again"
    );

    // A turn that ends on an account with headroom holds nothing and moves nothing.
    s.drive.router_turn_started(ROUTER, "t3");
    s.host.turns(ROUTER, vec![turn("t3", "completed", &[])]);
    s.drive.router_turn_ended(ROUTER).await?;
    assert!(!s.drive.holds_queue(ROUTER));
    s.drive.router_turn_boundary(ROUTER, Some("t3")).await?;
    assert_eq!(s.host.log().len(), 2);
    Ok(())
}

#[tokio::test]
async fn the_router_moves_before_the_engine_hands_it_anything() -> anyhow::Result<()> {
    let s = setup().await?;
    s.host.on(ROUTER, "router-acct");
    s.host.on(SESSION, "router-acct");
    s.host.turns(ROUTER, vec![turn("t4", "completed", &[])]);
    s.pool.set(vec![
        account("router-acct", "pro", 9.0, 2),
        account("next", "pro", 60.0, 5),
    ]);
    let gate = Arc::new(OnceLock::new());
    let _ = gate.set(s.drive.delivery_gate());
    let engine_host = LedgerHost::with_gate(
        s.host.clone() as Arc<dyn SuperagentHost>,
        Arc::new(DeliveryLedger::default()),
        gate,
    );
    engine_host
        .deliver(
            ThreadId::from_string(ROUTER)?,
            "[通知] 推进 · 整理笔记 · 交了结论".to_owned(),
            DeliveryMode::Queue,
        )
        .await?;
    // A session that is not a Router passes as it is.
    engine_host
        .deliver(
            ThreadId::from_string(SESSION)?,
            "接着做".to_owned(),
            DeliveryMode::Queue,
        )
        .await?;
    assert_eq!(
        s.host.log(),
        vec![
            format!("bind:{ROUTER}:next"),
            format!("deliver:{ROUTER}:Queue"),
            format!("deliver:{SESSION}:Queue"),
        ]
    );
    assert_eq!(s.host.account(SESSION).as_deref(), Some("router-acct"));
    Ok(())
}

#[tokio::test]
async fn a_router_turn_its_account_failed_moves_once_and_its_words_come_back() -> anyhow::Result<()>
{
    let s = setup().await?;
    s.host.on(ROUTER, "router-acct");
    s.pool.set(vec![
        account("router-acct", "pro", 60.0, 2),
        account("next", "pro", 80.0, 5),
    ]);
    let words = "看看 solidcraft 上游最近有什么新进展";
    s.drive.router_turn_started(ROUTER, "t5");
    s.drive
        .router_turn_troubled(ROUTER, "t5", Trouble::UsageLimit);
    // The pool still shows quota, but the account failed the turn: the queue waits.
    s.drive.router_turn_ended(ROUTER).await?;
    assert!(s.drive.holds_queue(ROUTER));
    s.host.turns(
        ROUTER,
        vec![failed(
            "t5",
            "usageLimitExceeded",
            "You've hit your usage limit.",
            &[words],
        )],
    );
    s.drive.router_turn_boundary(ROUTER, Some("t5")).await?;
    assert_eq!(
        s.host.log(),
        vec![
            format!("bind:{ROUTER}:next"),
            format!("deliver:{ROUTER}:Steer"),
            format!("start_queued:{ROUTER}")
        ]
    );
    let sent = s.host.delivered();
    assert!(
        sent[0].starts_with("[通知] Router 的账号账号额度用完（router-acct），已换到 next")
            && sent[0].ends_with(words),
        "{}",
        sent[0]
    );
    // Cooling down until its quota resets, with what it showed when it ran out.
    let reset = lock(&s.pool.accounts)[0].resets_at;
    assert_eq!(
        s.store.kv_get::<serde_json::Value>(COOLDOWN_KEY).await?,
        Some(json!({ "router-acct": {
            "until": reset, "reason": "账号额度用完", "kind": "usage_limit", "remaining": 60.0,
        }}))
    );
    assert_eq!(
        moves(&s.drive).await?,
        vec![moved(ROUTER, "router-acct", "next", "Router 账号额度用完")]
    );
    // The same turn looked at again moves nothing.
    s.drive.router_turn_boundary(ROUTER, Some("t5")).await?;
    s.drive.router_turn_failed(ROUTER).await?;
    assert_eq!(s.host.log().len(), 3);
    Ok(())
}

#[tokio::test]
async fn a_router_turn_shown_interrupted_without_its_error_still_moves_the_router()
-> anyhow::Result<()> {
    let s = setup().await?;
    s.host.on(ROUTER, "router-acct");
    s.pool.set(vec![
        account("router-acct", "pro", 60.0, 2),
        account("next", "pro", 80.0, 5),
    ]);
    let words = "第 15 路交了结论，接着安排";
    s.drive.router_turn_started(ROUTER, "t7");
    s.drive
        .router_turn_troubled(ROUTER, "t7", Trouble::UsageLimit);
    s.drive.router_turn_ended(ROUTER).await?;
    // The history shows the turn interrupted and without its error: the error hook said it.
    s.host
        .turns(ROUTER, vec![turn("t7", "interrupted", &[words])]);
    s.drive.router_turn_boundary(ROUTER, Some("t7")).await?;
    assert_eq!(
        s.host.log(),
        vec![
            format!("bind:{ROUTER}:next"),
            format!("deliver:{ROUTER}:Steer"),
            format!("start_queued:{ROUTER}")
        ]
    );
    assert!(s.host.delivered()[0].ends_with(words));
    Ok(())
}

#[tokio::test]
async fn a_failed_turn_behind_a_newer_one_moves_the_router_once_that_one_ends() -> anyhow::Result<()>
{
    let s = setup().await?;
    s.host.on(ROUTER, "router-acct");
    s.pool.set(vec![
        account("router-acct", "pro", 60.0, 2),
        account("next", "pro", 80.0, 5),
    ]);
    let capacity = "Selected model is at capacity. Please try a different model.";
    // The person typed again before the failed turn was looked at.
    s.host.turns(
        ROUTER,
        vec![
            turn("t7", "inProgress", &["还在吗"]),
            failed("t6", "serverOverloaded", capacity, &["整理一下今天的进展"]),
        ],
    );
    let busy = s.drive.router_turn_boundary(ROUTER, Some("t6")).await;
    assert!(busy.is_err(), "no move while a turn runs");
    assert_eq!(s.host.log(), Vec::<String>::new());

    s.host.turns(
        ROUTER,
        vec![
            turn("t7", "completed", &["还在吗"]),
            failed("t6", "serverOverloaded", capacity, &["整理一下今天的进展"]),
        ],
    );
    s.drive.router_turn_boundary(ROUTER, Some("t6")).await?;
    assert_eq!(
        s.host.log(),
        vec![
            format!("bind:{ROUTER}:next"),
            format!("deliver:{ROUTER}:Steer")
        ]
    );
    assert!(s.host.delivered()[0].ends_with("整理一下今天的进展"));
    Ok(())
}

#[test]
fn a_router_turn_is_stuck_after_two_capacity_or_usage_incidents_within_the_window() {
    let policy = AccountPolicy::default();
    let now = chrono::Utc::now();
    let incident = |turn: &str, kind: &str, secs_ago: i64| Incident {
        at: (now - chrono::Duration::seconds(secs_ago))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        thread: ROUTER.to_owned(),
        turn: Some(turn.to_owned()),
        kind: kind.to_owned(),
        message: "Reconnecting... 2/5; Selected model is at capacity".to_owned(),
    };
    let stuck = vec![
        incident("t8", "capacity", 100),
        incident("t8", "capacity", 5),
    ];
    let not_yet = vec![
        incident("t8", "capacity", 5),
        // a rate limit passes by itself
        incident("t8", "rate_limit", 4),
        // another turn
        incident("t0", "capacity", 3),
        // outside the window
        incident("t8", "capacity", 400),
    ];
    let quota = vec![
        incident("t8", "capacity", 50),
        incident("t8", "usage_limit", 5),
    ];
    assert_eq!(
        (
            rescue_due(&stuck, ROUTER, "t8", &policy, now),
            rescue_due(&not_yet, ROUTER, "t8", &policy, now),
            rescue_due(&quota, ROUTER, "t8", &policy, now),
            rescue_due(&stuck, SESSION, "t8", &policy, now),
        ),
        (
            Some(Trouble::Capacity),
            None,
            Some(Trouble::UsageLimit),
            None
        )
    );
}

#[tokio::test]
async fn a_router_turn_stuck_retrying_is_stopped_moved_and_handed_its_words_once()
-> anyhow::Result<()> {
    let s = setup().await?;
    s.host.on(ROUTER, "router-acct");
    s.pool.set(vec![
        account("router-acct", "pro", 60.0, 2),
        account("next", "pro", 80.0, 5),
    ]);
    let words = "看看上游有什么新进展";
    s.host
        .turns(ROUTER, vec![turn("t9", "inProgress", &[words])]);
    s.drive
        .rescue_router(ROUTER, "t9", Trouble::Capacity)
        .await?;
    // The queue waits while the Router moves, and starts once the words are back.
    assert_eq!(
        s.host.log(),
        vec![
            format!("interrupt:{ROUTER}"),
            format!("bind:{ROUTER}:next"),
            format!("deliver:{ROUTER}:Steer"),
            format!("start_queued:{ROUTER}")
        ]
    );
    assert!(!s.drive.holds_queue(ROUTER));
    let sent = s.host.delivered();
    assert!(
        sent[0].starts_with(
            "[通知] Router 的账号模型满载（Selected model is at capacity），刚才那一轮一直在重试（router-acct），已停下并换到 next"
        ) && sent[0].ends_with(words),
        "{}",
        sent[0]
    );
    let until = s
        .drive
        .cooldowns()
        .await?
        .get("router-acct")
        .map(|cooldown| cooldown.until - now());
    assert!(
        until.is_some_and(|left| (14 * 60..=15 * 60).contains(&left)),
        "{until:?}"
    );

    // The stopped turn's end, a second rescue and the failed-turn path move nothing more.
    s.drive.router_turn_aborted(ROUTER).await?;
    s.drive.router_turn_boundary(ROUTER, Some("t9")).await?;
    s.drive
        .rescue_router(ROUTER, "t9", Trouble::Capacity)
        .await?;
    s.drive.router_turn_failed(ROUTER).await?;
    assert_eq!(s.host.log().len(), 4);
    Ok(())
}

#[tokio::test]
async fn without_a_free_account_a_stuck_turn_is_left_to_retry() -> anyhow::Result<()> {
    let s = setup().await?;
    s.host.on(ROUTER, "router-acct");
    s.pool.set(vec![
        account("router-acct", "pro", 60.0, 2),
        account("free", "free", 100.0, 1),
        account("low", "pro", 2.0, 1),
    ]);
    s.host
        .turns(ROUTER, vec![turn("t10", "inProgress", &["在吗"])]);
    s.drive
        .rescue_router(ROUTER, "t10", Trouble::Capacity)
        .await?;
    // A turn that got through after all is left alone too.
    s.pool.set(vec![
        account("router-acct", "pro", 60.0, 2),
        account("next", "pro", 80.0, 5),
    ]);
    s.host
        .turns(ROUTER, vec![turn("t11", "completed", &["在吗"])]);
    s.drive
        .rescue_router(ROUTER, "t11", Trouble::Capacity)
        .await?;
    assert_eq!(s.host.log(), Vec::<String>::new());
    assert!(s.drive.cooldowns().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn two_capacity_retries_of_a_router_turn_ask_for_its_rescue() -> anyhow::Result<()> {
    let s = setup().await?;
    let dir = TempDir::new()?;
    incidents::install(dir.path());
    incidents::limit_to(s.drive.incident_scope());
    incidents::subscribe(s.drive.incident_listener());
    let retry = |thread: &str, n: u32| {
        incidents::note(
            thread,
            Some("t12"),
            &format!("Reconnecting... {n}/5"),
            Some("Selected model is at capacity. Please try a different model."),
        );
    };
    retry(ROUTER, 1);
    // A session that is not a Router is not even recorded.
    retry(SESSION, 1);
    retry(SESSION, 2);
    assert_eq!(s.effects.queued(KIND_RESCUE).await?, Vec::new());
    retry(ROUTER, 2);
    wait_until_async("the rescue is asked for", || async {
        !s.effects
            .queued(KIND_RESCUE)
            .await
            .unwrap_or_default()
            .is_empty()
    })
    .await;
    assert_eq!(
        s.effects.queued(KIND_RESCUE).await?,
        vec![(
            ROUTER.to_owned(),
            json!({ "turn": "t12", "trouble": "capacity" })
        )]
    );
    let recorded: Vec<String> = incidents::recent()
        .into_iter()
        .map(|incident| incident.thread)
        .collect();
    assert_eq!(recorded, vec![ROUTER.to_owned(), ROUTER.to_owned()]);
    Ok(())
}

#[tokio::test]
async fn a_step_goes_out_on_an_account_with_headroom_and_never_moves_midway() -> anyhow::Result<()>
{
    let s = setup().await?;
    s.host.on(SESSION, "low-acct");
    s.pool.set(vec![
        account("low-acct", "pro", 6.0, 2),
        account("fresh", "pro", 70.0, 5),
    ]);
    let record = s.drive.start(SESSION, simple(s.work.path(), None)).await?;
    assert_eq!(
        s.host.log(),
        vec![format!("bind:{SESSION}:fresh"), format!("step:{SESSION}")]
    );
    let record = s.drive.record(SESSION).await?.unwrap_or(record);
    assert_eq!(record.account.as_deref(), Some("fresh"));
    assert_eq!(
        moves(&s.drive).await?,
        vec![moved(SESSION, "low-acct", "fresh", "第 1 步前原账号剩 6%")]
    );
    // The step reports.
    s.drive.turn_started(SESSION).await?;
    s.host.turns(
        SESSION,
        vec![TurnBrief {
            assistant: vec!["分好了，按主题建了五个子目录".to_owned()],
            ..turn("s1", "completed", &[])
        }],
    );
    s.drive.turn_stopped(SESSION).await?;
    s.drive.step(SESSION).await?;
    assert!(matches!(
        s.drive.record(SESSION).await?.map(|record| record.state),
        Some(DriveState::Reported { .. })
    ));
    // The person typed into the session meanwhile and its turn runs: the next step does not move
    // it, although its account ran low.
    s.pool.set(vec![
        account("fresh", "pro", 4.0, 5),
        account("other", "pro", 70.0, 9),
    ]);
    s.host.turns(
        SESSION,
        vec![turn("s2", "inProgress", &["顺便把旧文件删了"])],
    );
    let say = Say {
        text: "再把每个子目录写一份说明。".to_owned(),
        builds_on: "按主题建了五个子目录".to_owned(),
        serves: "按主题分好".to_owned(),
        ..Say::default()
    };
    s.drive.say(SESSION, say).await?;
    assert_eq!(
        s.host.log(),
        vec![
            format!("bind:{SESSION}:fresh"),
            format!("step:{SESSION}"),
            format!("step:{SESSION}")
        ]
    );
    assert_eq!(
        (s.pool.full_reads(), s.pool.own_reads()),
        (1, vec!["low-acct".to_owned()]),
        "a running session is not looked at"
    );
    Ok(())
}

#[tokio::test]
async fn a_move_passes_by_an_account_that_carries_its_share_of_sessions() -> anyhow::Result<()> {
    let s = setup_with(AccountPolicy {
        max_per_account: 2,
        ..AccountPolicy::default()
    })
    .await?;
    s.pool.set(vec![
        account("low-acct", "pro", 6.0, 2),
        account("soonest", "pro", 80.0, 3),
        account("later", "pro", 80.0, 9),
    ]);
    // The Router and one session at work run on the account that resets soonest.
    s.host.on(ROUTER, "soonest");
    s.drive
        .start(OPENED, simple(s.work.path(), Some("soonest")))
        .await?;
    // A session whose account runs low moves before its step, past that full account.
    s.host.on(SESSION, "low-acct");
    s.drive.start(SESSION, simple(s.work.path(), None)).await?;
    assert_eq!(s.host.account(SESSION).as_deref(), Some("later"));
    // With room on every account, the soonest reset wins again.
    s.drive.stop(OPENED).await?;
    s.host.on(SESSION, "low-acct");
    s.drive.stop(SESSION).await?;
    s.drive.start(SESSION, simple(s.work.path(), None)).await?;
    assert_eq!(s.host.account(SESSION).as_deref(), Some("soonest"));
    Ok(())
}

#[tokio::test]
async fn a_drive_started_on_a_named_account_runs_there_from_its_first_step() -> anyhow::Result<()> {
    let s = setup().await?;
    s.pool.set(vec![
        account("router-acct", "pro", 60.0, 2),
        account("named", "pro", 6.0, 3),
        account("fresh", "pro", 70.0, 5),
    ]);
    // A new session opens under the account named (in any case), and its first step stays
    // there although that account runs low.
    let folder = s.work.path().join("02-notes");
    let refused = s.drive.open_session_on(&folder, "nobody").await;
    assert!(format!("{:#}", refused.expect_err("unknown account")).contains("账号池里没有 nobody"));
    let opened = s.drive.open_session_on(&folder, "NAMED").await?;
    assert_eq!(opened, OPENED);
    assert_eq!(
        lock(&s.host.opened).clone(),
        vec![(folder.clone(), Some("named".to_owned()))]
    );
    let record = s
        .drive
        .start(OPENED, simple(&folder, Some("NAMED")))
        .await?;
    assert_eq!(record.account.as_deref(), Some("named"));
    assert_eq!(s.host.log(), vec![format!("step:{OPENED}")]);

    // A session that exists moves to the account named before its first step.
    s.host.on(SESSION, "router-acct");
    let record = s
        .drive
        .start(SESSION, simple(s.work.path(), Some("fresh")))
        .await?;
    assert_eq!(record.account.as_deref(), Some("fresh"));
    assert_eq!(
        s.host.log(),
        vec![
            format!("step:{OPENED}"),
            format!("bind:{SESSION}:fresh"),
            format!("step:{SESSION}")
        ]
    );
    assert_eq!(
        moves(&s.drive).await?,
        vec![moved(
            SESSION,
            "router-acct",
            "fresh",
            "推进开始时指定的账号"
        )]
    );
    Ok(())
}

#[tokio::test]
async fn a_refilled_account_ends_its_quota_cooldown_on_a_pool_read_and_is_taken_in_that_look()
-> anyhow::Result<()> {
    let s = setup().await?;
    // It ran out days ago and was to cool down until its old reset; the person refilled it since.
    let days = now() + 5 * 24 * 3600;
    seed_cooldowns(
        &s,
        &BTreeMap::from([("refilled".to_owned(), out_of_quota(days, 0.0))]),
    )
    .await?;
    s.host.on(ROUTER, "router-acct");
    s.host.turns(ROUTER, vec![turn("t1", "completed", &[])]);
    s.pool.set(vec![
        account("router-acct", "pro", 8.0, 2),
        account("refilled", "pro", 22.0, 3),
        account("later", "pro", 90.0, 40),
    ]);
    // The Router runs low: the read of the pool that finds it an account ends the cooldown, and
    // the refilled account, whose quota resets soonest, is taken in the same look.
    s.drive.watch_accounts().await;
    assert_eq!(s.host.log(), vec![format!("bind:{ROUTER}:refilled")]);
    assert_eq!(
        moves(&s.drive).await?,
        vec![moved(
            ROUTER,
            "router-acct",
            "refilled",
            "Router 原账号剩 8%，提前换号"
        )]
    );
    assert_eq!(s.drive.cooldowns().await?, BTreeMap::new());
    assert_eq!(
        ended(&s.drive).await?,
        vec![ended_early("refilled", "额度恢复（剩 22%），提前结束冷却")]
    );
    Ok(())
}

#[tokio::test]
async fn a_refilled_account_ends_its_quota_cooldown_on_its_own_read_and_the_step_stays_on_it()
-> anyhow::Result<()> {
    let s = setup().await?;
    let days = now() + 5 * 24 * 3600;
    seed_cooldowns(
        &s,
        &BTreeMap::from([("session-acct".to_owned(), out_of_quota(days, 0.0))]),
    )
    .await?;
    s.host.on(SESSION, "session-acct");
    s.pool.set(vec![
        account("session-acct", "pro", 50.0, 2),
        account("other", "pro", 70.0, 1),
    ]);
    // The check before step 1 reads the session's account on its own: its quota is back, so its
    // cooldown ends and the step goes out there, the pool never read.
    s.drive.start(SESSION, simple(s.work.path(), None)).await?;
    assert_eq!(
        (s.host.log(), s.pool.full_reads(), s.pool.own_reads()),
        (
            vec![format!("step:{SESSION}")],
            0,
            vec!["session-acct".to_owned()]
        )
    );
    assert_eq!(s.drive.cooldowns().await?, BTreeMap::new());
    assert_eq!(
        ended(&s.drive).await?,
        vec![ended_early(
            "session-acct",
            "额度恢复（剩 50%），提前结束冷却"
        )]
    );
    Ok(())
}

#[tokio::test]
async fn a_quota_cooldown_stays_while_the_reads_do_not_show_the_quota_back() -> anyhow::Result<()> {
    let s = setup().await?;
    let until = now() + 3 * 24 * 3600;
    let cooling = BTreeMap::from([
        ("empty".to_owned(), out_of_quota(until, 0.0)),
        ("below-soft".to_owned(), out_of_quota(until, 0.0)),
        ("unusable".to_owned(), out_of_quota(until, 0.0)),
        // Its read showed this much when a turn ran out of quota on it: what ran out does not
        // show in its reads.
        ("unseen-limit".to_owned(), out_of_quota(until, 60.0)),
    ]);
    seed_cooldowns(&s, &cooling).await?;
    s.host.on(ROUTER, "router-acct");
    s.host.turns(ROUTER, vec![turn("t1", "completed", &[])]);
    s.pool.set(vec![
        account("router-acct", "pro", 8.0, 2),
        account("empty", "pro", 0.0, 1),
        account("below-soft", "pro", 8.0, 1),
        AccountInfo {
            usable: false,
            ..account("unusable", "pro", 100.0, 1)
        },
        account("unseen-limit", "pro", 60.0, 1),
        account("next", "pro", 70.0, 30),
    ]);
    s.drive.watch_accounts().await;
    // Every cooldown stands; the Router moves to the one account free of them.
    assert_eq!(s.host.log(), vec![format!("bind:{ROUTER}:next")]);
    assert_eq!(s.drive.cooldowns().await?, cooling);
    assert_eq!(ended(&s.drive).await?, Vec::new());
    Ok(())
}

#[tokio::test]
async fn a_capacity_cooldown_runs_its_time_whatever_the_quota_reads() -> anyhow::Result<()> {
    let s = setup().await?;
    let cooling = BTreeMap::from([(
        "busy".to_owned(),
        Cooldown {
            until: now() + 600,
            reason: "模型满载（Selected model is at capacity）".to_owned(),
            kind: Some(Trouble::Capacity),
            remaining: Some(40.0),
        },
    )]);
    seed_cooldowns(&s, &cooling).await?;
    s.host.on(ROUTER, "router-acct");
    s.host.turns(ROUTER, vec![turn("t1", "completed", &[])]);
    s.pool.set(vec![
        account("router-acct", "pro", 8.0, 2),
        account("busy", "pro", 95.0, 1),
        account("next", "pro", 70.0, 30),
    ]);
    s.drive.watch_accounts().await;
    assert_eq!(s.host.log(), vec![format!("bind:{ROUTER}:next")]);
    assert_eq!(s.drive.cooldowns().await?, cooling);
    assert_eq!(ended(&s.drive).await?, Vec::new());
    Ok(())
}

#[tokio::test]
async fn cooldown_rows_of_earlier_versions_are_known_by_their_reason() -> anyhow::Result<()> {
    let s = setup().await?;
    let later = now() + 3 * 24 * 3600;
    let soon = now() + 600;
    let capacity = "模型满载（Selected model is at capacity）";
    // Rows as earlier versions wrote them: only `until` and `reason`.
    seed_cooldowns(
        &s,
        &json!({
            "quota-gone": { "until": later, "reason": "账号额度用完" },
            "busy": { "until": soon, "reason": capacity },
            "unknown": { "until": later, "reason": "人工停用" },
            "over": { "until": now() - 60, "reason": "账号额度用完" },
        }),
    )
    .await?;
    let legacy = |until: i64, reason: &str| Cooldown {
        until,
        reason: reason.to_owned(),
        kind: None,
        remaining: None,
    };
    let cooling = s.drive.cooldowns().await?;
    assert_eq!(
        cooling,
        BTreeMap::from([
            ("busy".to_owned(), legacy(soon, capacity)),
            ("quota-gone".to_owned(), legacy(later, "账号额度用完")),
            ("unknown".to_owned(), legacy(later, "人工停用")),
        ])
    );
    assert_eq!(
        cooling
            .iter()
            .map(|(name, cooldown)| (name.as_str(), cooldown.trouble()))
            .collect::<Vec<_>>(),
        vec![
            ("busy", Some(Trouble::Capacity)),
            ("quota-gone", Some(Trouble::UsageLimit)),
            ("unknown", None),
        ]
    );

    // A read that shows quota on all of them ends only the usage-limit one, which the Router,
    // running low, takes at once.
    s.host.on(ROUTER, "router-acct");
    s.host.turns(ROUTER, vec![turn("t1", "completed", &[])]);
    s.pool.set(vec![
        account("router-acct", "pro", 8.0, 2),
        account("quota-gone", "pro", 50.0, 3),
        account("busy", "pro", 90.0, 1),
        account("unknown", "pro", 90.0, 1),
        account("next", "pro", 70.0, 30),
    ]);
    s.drive.watch_accounts().await;
    assert_eq!(s.host.log(), vec![format!("bind:{ROUTER}:quota-gone")]);
    assert_eq!(
        s.store.kv_get::<serde_json::Value>(COOLDOWN_KEY).await?,
        Some(json!({
            "busy": { "until": soon, "reason": capacity, "kind": null, "remaining": null },
            "unknown": { "until": later, "reason": "人工停用", "kind": null, "remaining": null },
        }))
    );
    assert_eq!(
        ended(&s.drive).await?,
        vec![ended_early(
            "quota-gone",
            "额度恢复（剩 50%），提前结束冷却"
        )]
    );
    Ok(())
}
