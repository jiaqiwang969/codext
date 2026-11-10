//! The round's look at the machine: the main Router hears once when the volume that holds
//! `CODEX_HOME` falls below the threshold, again at most daily while it stays below, and at once
//! again only after the free space came back a point above it. The disk's numbers are made up:
//! no test reads a real disk.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::DateTime;
use chrono::SecondsFormat;
use chrono::TimeDelta;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_superagent_extension::BoxFut;
use codex_superagent_extension::Effects;
use codex_superagent_extension::HealthBoard;
use codex_superagent_extension::Store;
use codex_superagent_extension::drive::AccountPolicy;
use codex_superagent_extension::drive::Audit;
use codex_superagent_extension::drive::DISK_FREE_PERCENT_VAR;
use codex_superagent_extension::drive::DiskSpace;
use codex_superagent_extension::drive::DiskState;
use codex_superagent_extension::drive::DiskVerdict;
use codex_superagent_extension::drive::DriveOptions;
use codex_superagent_extension::drive::DriveService;
use codex_superagent_extension::drive::Judge;
use codex_superagent_extension::drive::KIND_NOTICE;
use codex_superagent_extension::drive::MachineStatus;
use codex_superagent_extension::drive::MachineWatch;
use codex_superagent_extension::drive::REPEAT_EVERY;
use codex_superagent_extension::host::DeliveryMode;
use codex_superagent_extension::host::GoalStatus;
use codex_superagent_extension::host::SuperagentHost;
use codex_superagent_extension::host::ThreadSpec;
use codex_superagent_extension::pm_threads::PmThreads;
use common::FakeClock;
use common::T0;
use common::open_store;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

const GB: u64 = 1_000_000_000;
/// A 1000 GB volume, so 10 GB free is one percent.
const TOTAL: u64 = 1_000 * GB;
const THRESHOLD: f64 = 5.0;
const OFF: f64 = 0.0;
const MAIN: &str = "019a0000-0000-7000-8000-00000000f0aa";
const PROJECT: &str = "019a0000-0000-7000-8000-00000000f0bb";

/// 2026-11-10T08:00:00Z: when the disk first runs low here.
fn morning() -> DateTime<Utc> {
    DateTime::<Utc>::UNIX_EPOCH + TimeDelta::seconds(1_794_297_600)
}

fn iso(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// The state after the main Router was told at `when`, the disk then `free_gb` GB free.
fn told_at(free_gb: u64, when: DateTime<Utc>) -> DiskState {
    DiskState {
        machine: MachineStatus {
            free_bytes: free_gb * GB,
            total_bytes: TOTAL,
            free_percent: free_gb as f64 / 10.0,
            checked_at: Some(iso(when)),
            last_notice_at: Some(iso(when)),
        },
        low: true,
    }
}

#[test]
fn a_disk_below_the_threshold_is_told_once() {
    let first = morning();
    let low = DiskSpace {
        free_bytes: 40 * GB,
        total_bytes: TOTAL,
    };
    let (state, verdict) = DiskState::default().look(low, THRESHOLD, first);
    assert_eq!(
        (state.clone(), verdict),
        (
            DiskState {
                machine: MachineStatus {
                    free_bytes: 40 * GB,
                    total_bytes: TOTAL,
                    free_percent: 4.0,
                    checked_at: Some(iso(first)),
                    last_notice_at: Some(iso(first)),
                },
                low: true,
            },
            DiskVerdict::Notify,
        )
    );

    // Still below an hour later: the reading is kept, nothing is told.
    let later = first + TimeDelta::hours(1);
    let lower = DiskSpace {
        free_bytes: 35 * GB,
        total_bytes: TOTAL,
    };
    assert_eq!(
        state.look(lower, THRESHOLD, later),
        (
            DiskState {
                machine: MachineStatus {
                    free_bytes: 35 * GB,
                    total_bytes: TOTAL,
                    free_percent: 3.5,
                    checked_at: Some(iso(later)),
                    last_notice_at: Some(iso(first)),
                },
                low: true,
            },
            DiskVerdict::Quiet,
        )
    );
}

#[test]
fn a_disk_at_or_above_the_threshold_is_not_told() {
    let now = morning();
    let at_threshold = DiskSpace {
        free_bytes: 50 * GB,
        total_bytes: TOTAL,
    };
    assert_eq!(
        DiskState::default().look(at_threshold, THRESHOLD, now),
        (
            DiskState {
                machine: MachineStatus {
                    free_bytes: 50 * GB,
                    total_bytes: TOTAL,
                    free_percent: 5.0,
                    checked_at: Some(iso(now)),
                    last_notice_at: None,
                },
                low: false,
            },
            DiskVerdict::Quiet,
        )
    );
}

#[test]
fn a_disk_that_stays_low_is_told_again_a_day_after_the_last_notice() {
    let first = morning();
    let state = told_at(/*free_gb*/ 40, first);
    let low = DiskSpace {
        free_bytes: 30 * GB,
        total_bytes: TOTAL,
    };

    let almost = first + REPEAT_EVERY - TimeDelta::minutes(1);
    assert_eq!(
        state.look(low, THRESHOLD, almost),
        (
            DiskState {
                machine: MachineStatus {
                    free_bytes: 30 * GB,
                    total_bytes: TOTAL,
                    free_percent: 3.0,
                    checked_at: Some(iso(almost)),
                    last_notice_at: Some(iso(first)),
                },
                low: true,
            },
            DiskVerdict::Quiet,
        )
    );

    let day = first + REPEAT_EVERY;
    assert_eq!(
        state.look(low, THRESHOLD, day),
        (
            DiskState {
                machine: MachineStatus {
                    free_bytes: 30 * GB,
                    total_bytes: TOTAL,
                    free_percent: 3.0,
                    checked_at: Some(iso(day)),
                    last_notice_at: Some(iso(day)),
                },
                low: true,
            },
            DiskVerdict::Notify,
        )
    );
}

#[test]
fn only_a_point_above_the_threshold_rearms_the_notice() {
    let first = morning();
    let space = |free_gb: u64| DiskSpace {
        free_bytes: free_gb * GB,
        total_bytes: TOTAL,
    };
    let status = |free_gb: u64, checked: DateTime<Utc>, told: DateTime<Utc>| MachineStatus {
        free_bytes: free_gb * GB,
        total_bytes: TOTAL,
        free_percent: free_gb as f64 / 10.0,
        checked_at: Some(iso(checked)),
        last_notice_at: Some(iso(told)),
    };

    // Back above the threshold, but less than a point above it: still low.
    let hour = first + TimeDelta::hours(1);
    let (state, verdict) = told_at(/*free_gb*/ 40, first).look(space(55), THRESHOLD, hour);
    assert_eq!(
        (state.clone(), verdict),
        (
            DiskState {
                machine: status(55, hour, first),
                low: true,
            },
            DiskVerdict::Quiet,
        )
    );

    // Falling below again within the day is no first fall: nothing is told.
    let two = first + TimeDelta::hours(2);
    let (state, verdict) = state.look(space(45), THRESHOLD, two);
    assert_eq!(
        (state.clone(), verdict),
        (
            DiskState {
                machine: status(45, two, first),
                low: true,
            },
            DiskVerdict::Quiet,
        )
    );

    // A point above the threshold: the notice re-arms.
    let three = first + TimeDelta::hours(3);
    let (state, verdict) = state.look(space(60), THRESHOLD, three);
    assert_eq!(
        (state.clone(), verdict),
        (
            DiskState {
                machine: status(60, three, first),
                low: false,
            },
            DiskVerdict::Rearm,
        )
    );

    // The next fall is told at once, though the last notice is less than a day old.
    let four = first + TimeDelta::hours(4);
    assert_eq!(
        state.look(space(40), THRESHOLD, four),
        (
            DiskState {
                machine: status(40, four, four),
                low: true,
            },
            DiskVerdict::Notify,
        )
    );
}

#[test]
fn a_zero_threshold_turns_the_look_off() {
    let now = morning();
    let nearly_full = DiskSpace {
        free_bytes: GB,
        total_bytes: TOTAL,
    };
    assert_eq!(
        DiskState::default().look(nearly_full, OFF, now),
        (DiskState::default(), DiskVerdict::Quiet)
    );
    // Not even a told state re-arms or repeats.
    let state = told_at(/*free_gb*/ 40, now - REPEAT_EVERY * 2);
    assert_eq!(
        state.look(nearly_full, OFF, now),
        (state, DiskVerdict::Quiet)
    );
}

/// Only unix systems have the look at all.
#[cfg(unix)]
#[test]
fn the_threshold_comes_from_the_daemon_environment() {
    let threshold = |value: Option<&str>| {
        MachineWatch::for_home(
            Path::new("/codex-home"),
            Arc::new(|| Some(MAIN.to_owned())),
            |name| {
                assert_eq!(name, DISK_FREE_PERCENT_VAR);
                value.map(str::to_owned)
            },
        )
        .map(|watch| watch.threshold)
    };
    assert_eq!(
        [
            None,
            Some(""),
            Some("lots"),
            Some("120"),
            Some(" 7.5 "),
            Some("99"),
            Some("0"),
        ]
        .map(threshold),
        [
            Some(5.0),
            Some(5.0),
            Some(5.0),
            Some(5.0),
            Some(7.5),
            Some(99.0),
            None,
        ]
    );
}

#[test]
fn the_main_router_is_the_first_registered_without_a_project() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let routers = PmThreads::new(dir.path());
    std::fs::write(
        routers.file(),
        format!("# Routers\n{PROJECT} project=251 repo=/tmp/251\n{MAIN} account=Lab\nlater\n"),
    )?;
    assert_eq!(routers.main_router(), Some(MAIN.to_owned()));
    std::fs::write(routers.file(), format!("{PROJECT} project=251\n"))?;
    assert_eq!(routers.main_router(), None);
    Ok(())
}

/// A host that only takes deliveries.
#[derive(Default)]
struct Host {
    delivered: Mutex<Vec<(String, String)>>,
}

impl Host {
    fn delivered(&self) -> Vec<(String, String)> {
        self.delivered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl SuperagentHost for Host {
    fn start_thread(&self, _spec: ThreadSpec) -> BoxFut<'_, anyhow::Result<ThreadId>> {
        Box::pin(async { anyhow::bail!("no threads are started here") })
    }
    fn bind_profile(&self, _thread: ThreadId, _profile: String) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn resume_thread(
        &self,
        _thread: ThreadId,
        _role: Option<String>,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn deliver(
        &self,
        thread: ThreadId,
        text: String,
        _mode: DeliveryMode,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        self.delivered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((thread.to_string(), text));
        Box::pin(async { Ok(()) })
    }
    fn interrupt(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
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

struct NoJudge;

impl Judge for NoJudge {
    fn audit<'a>(&'a self, _project_home: &'a Path) -> BoxFut<'a, anyhow::Result<Audit>> {
        Box::pin(async { Ok(Audit::default()) })
    }
}

/// The disk the tests set, and how often the engine read it.
#[derive(Default)]
struct Disk {
    space: Mutex<Option<DiskSpace>>,
    reads: AtomicUsize,
}

impl Disk {
    fn set(&self, free_bytes: u64) {
        *self.space.lock().unwrap_or_else(PoisonError::into_inner) = Some(DiskSpace {
            free_bytes,
            total_bytes: TOTAL,
        });
    }
}

/// A drive on `store` as the daemon starts it: a project Router registered first, the main
/// Router second, the disk `disk`. Each call is a restart: new effects, nothing in memory.
async fn start(
    store: &Store,
    host: &Arc<Host>,
    disk: &Arc<Disk>,
    home: &Path,
) -> anyhow::Result<(Effects, DriveService)> {
    let effects =
        Effects::new(store.clone(), HealthBoard::new(), FakeClock::at(T0).clock()).await?;
    let reader = Arc::clone(disk);
    let drive = DriveService::install_with(
        store.clone(),
        effects.clone(),
        Arc::clone(host) as Arc<dyn SuperagentHost>,
        Arc::new(NoJudge),
        DriveOptions {
            accounts: None,
            stall: Duration::from_secs(3600),
            policy: AccountPolicy::default(),
            routers: Arc::new(|| vec![PROJECT.to_owned(), MAIN.to_owned()]),
            retry_after: Duration::ZERO,
            events: Some(home.join("events.jsonl")),
            machine: Some(MachineWatch {
                home: home.to_path_buf(),
                threshold: THRESHOLD,
                read: Arc::new(move |_home: &Path| -> std::io::Result<DiskSpace> {
                    reader.reads.fetch_add(1, Ordering::SeqCst);
                    let space = *reader.space.lock().unwrap_or_else(PoisonError::into_inner);
                    space.ok_or_else(|| std::io::Error::other("no disk"))
                }),
                main_router: Arc::new(|| Some(MAIN.to_owned())),
            }),
        },
    )
    .await?;
    Ok((effects, drive))
}

/// The events written so far, one JSON value per line.
fn events(home: &Path) -> anyhow::Result<Vec<Value>> {
    let text = std::fs::read_to_string(home.join("events.jsonl")).unwrap_or_default();
    let lines = text
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<Vec<Value>, _>>()?;
    Ok(lines)
}

#[tokio::test]
async fn the_main_router_hears_a_low_disk_once_and_a_restart_does_not_repeat_it()
-> anyhow::Result<()> {
    let (dir, store) = open_store().await?;
    let home = dir.path();
    let host = Arc::new(Host::default());
    let disk = Arc::new(Disk::default());
    disk.set(42 * GB);
    let (effects, drive) = start(&store, &host, &disk, home).await?;

    drive.sweep().await?;
    let summary = "这台电脑的数据盘只剩 42 GB 可用（4.2%），低于 5%。请按 ~/.codex/AGENTS.md「总 Router 的职责」：查清是什么占了空间，找到每一项的责任 Router，把清单和建议交给用户；删除要用户点明范围。";
    let queued: Vec<(String, Value, Value)> = effects
        .queued(KIND_NOTICE)
        .await?
        .into_iter()
        .map(|(key, payload)| (key, payload["router"].clone(), payload["summary"].clone()))
        .collect();
    assert_eq!(
        queued,
        vec![(format!("{MAIN}|machine:disk"), json!(MAIN), json!(summary))]
    );
    let seen = drive.machine_status().await?.expect("a reading");
    assert_eq!(
        seen,
        MachineStatus {
            free_bytes: 42 * GB,
            total_bytes: TOTAL,
            free_percent: 4.2,
            checked_at: seen.checked_at.clone(),
            last_notice_at: seen.checked_at.clone(),
        }
    );
    let lines = events(home)?;
    assert_eq!(
        lines,
        vec![json!({
            "at": lines[0]["at"],
            "event": "disk_low",
            "machine": seen,
        })]
    );

    // The notice reaches the main Router as a notice card.
    drive.deliver_notices(MAIN).await?;
    assert_eq!(
        host.delivered(),
        vec![(MAIN.to_owned(), format!("[通知] {summary}"))]
    );

    // The next round comes before the look is due: the disk is not read again.
    drive.sweep().await?;
    assert_eq!(disk.reads.load(Ordering::SeqCst), 1);

    // A restart reads the disk at once, but the Router was told just now.
    let (effects, drive) = start(&store, &host, &disk, home).await?;
    drive.sweep().await?;
    assert_eq!(disk.reads.load(Ordering::SeqCst), 2);
    assert_eq!(effects.queued(KIND_NOTICE).await?, Vec::new());
    assert_eq!(events(home)?.len(), 1);
    assert_eq!(
        drive
            .machine_status()
            .await?
            .and_then(|seen| seen.last_notice_at),
        seen.last_notice_at
    );

    // The space comes back a point above the threshold: the notice re-arms, nothing is told.
    disk.set(70 * GB);
    let (effects, drive) = start(&store, &host, &disk, home).await?;
    drive.sweep().await?;
    let back = drive.machine_status().await?.expect("a reading");
    assert_eq!(
        back,
        MachineStatus {
            free_bytes: 70 * GB,
            total_bytes: TOTAL,
            free_percent: 7.0,
            checked_at: back.checked_at.clone(),
            last_notice_at: seen.last_notice_at.clone(),
        }
    );
    let lines = events(home)?;
    assert_eq!(
        lines[1..].to_vec(),
        vec![json!({
            "at": lines[1]["at"],
            "event": "disk_rearmed",
            "machine": back,
        })]
    );
    assert_eq!(effects.queued(KIND_NOTICE).await?, Vec::new());
    assert_eq!(host.delivered().len(), 1);
    Ok(())
}
