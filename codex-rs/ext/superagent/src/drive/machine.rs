//! The round's look at the machine (`sweep.rs`): at most every `LOOK_EVERY`, the free space of
//! the volume that holds `CODEX_HOME`. When it falls below the threshold
//! (`CODEXT_SUPERAGENT_DISK_FREE_PERCENT`, 5 percent by default; 0 turns the look off), the main
//! Router, the one registered without a project, is told as a notice: once when the space first
//! falls below, then at most every `REPEAT_EVERY` while it stays below. A later fall counts as a
//! first one only after the space came back to `REARM_POINTS` above the threshold.
//!
//! The ledger keeps what the look last read and when it told the Router (`MACHINE_KEY`), so a
//! restart does not tell the Router again at once. `superagent/status` shows it as `machine`;
//! each notice and each re-arm is an event (`events.rs`).

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use chrono::DateTime;
use chrono::SecondsFormat;
use chrono::TimeDelta;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

use super::DriveService;

/// Below this percent of free space the main Router is told; 0 turns the look off.
pub const DISK_FREE_PERCENT_VAR: &str = "CODEXT_SUPERAGENT_DISK_FREE_PERCENT";
const DISK_FREE_PERCENT: f64 = 5.0;
/// The ledger key of what the look read and told.
pub const MACHINE_KEY: &str = "machine-disk";
/// How often the round reads the disk.
pub const LOOK_EVERY: Duration = Duration::from_secs(10 * 60);
/// How long a disk that stays low goes untold after a notice.
pub const REPEAT_EVERY: TimeDelta = TimeDelta::hours(24);
/// How far (percentage points) above the threshold the free space must come back before a later
/// fall is told at once again.
pub const REARM_POINTS: f64 = 1.0;
/// The notice's key among the notices about no session.
const NOTICE_KEY: &str = "machine:disk";
/// Bytes in a GB, as the system shows disk sizes.
const GB: f64 = 1e9;

/// The space of a volume, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskSpace {
    /// What a person may still write: the free space less the part kept for the system.
    pub free_bytes: u64,
    pub total_bytes: u64,
}

impl DiskSpace {
    /// The free part of the volume, in percent; a volume that reports no size counts as free.
    pub fn free_percent(self) -> f64 {
        if self.total_bytes == 0 {
            return 100.0;
        }
        self.free_bytes as f64 * 100.0 / self.total_bytes as f64
    }
}

/// Reads the space of the volume that holds a path.
pub type DiskReader = Arc<dyn Fn(&Path) -> std::io::Result<DiskSpace> + Send + Sync>;
/// The main Router, looked up on each use: the first one registered without a project.
pub type MainRouter = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// The space of the volume that holds `path`, as `statvfs` reports it.
#[cfg(unix)]
pub fn disk_space(path: &Path) -> std::io::Result<DiskSpace> {
    let stat = rustix::fs::statvfs(path)?;
    Ok(DiskSpace {
        free_bytes: stat.f_bavail.saturating_mul(stat.f_frsize),
        total_bytes: stat.f_blocks.saturating_mul(stat.f_frsize),
    })
}

/// The space of the volume that holds `path`: only unix systems report it here.
#[cfg(not(unix))]
pub fn disk_space(path: &Path) -> std::io::Result<DiskSpace> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "the space of the volume that holds {} is not read on this system",
            path.display()
        ),
    ))
}

/// What the round's look at the machine works with.
#[derive(Clone)]
pub struct MachineWatch {
    /// A path on the volume to watch: `CODEX_HOME`.
    pub home: PathBuf,
    /// Below this percent of free space the main Router is told.
    pub threshold: f64,
    /// Reads the volume's space (`disk_space`; tests give their own numbers).
    pub read: DiskReader,
    pub main_router: MainRouter,
}

impl MachineWatch {
    /// The look at the volume that holds `home` (`CODEX_HOME`), with the threshold `var` answers
    /// for `DISK_FREE_PERCENT_VAR` (unset, empty or not a percentage keeps the default), telling
    /// `main_router`. `None`, the look off, when the threshold is 0 or the system is not unix
    /// (`disk_space` reads no other).
    pub fn for_home(
        home: &Path,
        main_router: MainRouter,
        var: impl Fn(&str) -> Option<String>,
    ) -> Option<Self> {
        let threshold = var(DISK_FREE_PERCENT_VAR)
            .and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|percent| (0.0..=100.0).contains(percent))
            .unwrap_or(DISK_FREE_PERCENT);
        (threshold > 0.0 && cfg!(unix)).then(|| Self {
            home: home.to_path_buf(),
            threshold,
            read: Arc::new(disk_space),
            main_router,
        })
    }
}

/// The machine as `superagent/status` shows it and its events carry it.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MachineStatus {
    /// What a person may still write on the volume that holds `CODEX_HOME`, in bytes.
    pub free_bytes: u64,
    pub total_bytes: u64,
    /// `free_bytes` of `total_bytes`, in percent, to two decimals.
    pub free_percent: f64,
    /// When the engine last read the disk, RFC 3339.
    pub checked_at: Option<String>,
    /// When the engine last told the main Router the disk runs low, RFC 3339.
    pub last_notice_at: Option<String>,
}

/// What the engine keeps of its looks at the disk (`MACHINE_KEY`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct DiskState {
    pub machine: MachineStatus,
    /// The main Router was told, and the free space has not come back to `REARM_POINTS` above the
    /// threshold since: until it does, only the repeat goes out.
    pub low: bool,
}

/// What a look at the disk does besides keeping what it read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskVerdict {
    /// Nothing to tell.
    Quiet,
    /// Tell the main Router: the free space fell below the threshold, or it stays below
    /// `REPEAT_EVERY` after the last notice.
    Notify,
    /// The free space came back after a notice: the next fall below is told at once.
    Rearm,
}

impl DiskState {
    /// The look at `space`, read at `now`, with `threshold` the percent of free space below which
    /// the main Router is told (0 turns the look off and leaves the state as it is): the state
    /// after it and what to do.
    pub fn look(
        &self,
        space: DiskSpace,
        threshold: f64,
        now: DateTime<Utc>,
    ) -> (Self, DiskVerdict) {
        if threshold <= 0.0 {
            return (self.clone(), DiskVerdict::Quiet);
        }
        let percent = space.free_percent();
        let at = now.to_rfc3339_opts(SecondsFormat::Millis, true);
        let mut next = Self {
            machine: MachineStatus {
                free_bytes: space.free_bytes,
                total_bytes: space.total_bytes,
                free_percent: (percent * 100.0).round() / 100.0,
                checked_at: Some(at.clone()),
                last_notice_at: self.machine.last_notice_at.clone(),
            },
            low: self.low,
        };
        // An unreadable time counts as long ago; one ahead of `now` as just now.
        let repeat_due = self
            .machine
            .last_notice_at
            .as_deref()
            .and_then(|told| DateTime::parse_from_rfc3339(told).ok())
            .is_none_or(|told| now.signed_duration_since(told) >= REPEAT_EVERY);
        let verdict = if percent < threshold {
            if self.low && !repeat_due {
                DiskVerdict::Quiet
            } else {
                next.low = true;
                next.machine.last_notice_at = Some(at);
                DiskVerdict::Notify
            }
        } else if self.low && percent >= threshold + REARM_POINTS {
            next.low = false;
            DiskVerdict::Rearm
        } else {
            DiskVerdict::Quiet
        };
        (next, verdict)
    }
}

impl DriveService {
    /// What the engine last read of the machine, for `superagent/status`; `None` before its first
    /// look.
    pub async fn machine_status(&self) -> anyhow::Result<Option<MachineStatus>> {
        Ok(self
            .inner
            .store
            .kv_get::<DiskState>(MACHINE_KEY)
            .await?
            .map(|state| state.machine))
    }

    /// The round's look at the disk, once `LOOK_EVERY` passed since the last one: keeps what it
    /// read, and tells the main Router or re-arms as `DiskState::look` says. The notice and the
    /// state it changes are written together.
    pub(super) async fn look_at_machine(&self) -> anyhow::Result<()> {
        let Some(machine) = &self.inner.machine else {
            return Ok(());
        };
        {
            let mut looked = self
                .inner
                .machine_looked
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if looked.is_some_and(|at| at.elapsed() < LOOK_EVERY) {
                return Ok(());
            }
            *looked = Some(Instant::now());
        }
        let space = (machine.read)(&machine.home).with_context(|| {
            format!(
                "the space of the volume that holds {} is unreadable",
                machine.home.display()
            )
        })?;
        let state: DiskState = self
            .inner
            .store
            .kv_get(MACHINE_KEY)
            .await?
            .unwrap_or_default();
        let (next, verdict) = state.look(space, machine.threshold, Utc::now());
        self.inner
            .store
            .atomic(async |tx| {
                tx.kv_set(MACHINE_KEY, &next).await?;
                if verdict == DiskVerdict::Notify {
                    let gb = space.free_bytes as f64 / GB;
                    let free = if gb >= 10.0 {
                        format!("{gb:.0}")
                    } else {
                        format!("{gb:.1}")
                    };
                    // In tenths of a percent, rounded down, so the share shown is never at the
                    // threshold it is below.
                    let tenths = u128::from(space.free_bytes) * 1000
                        / u128::from(space.total_bytes.max(1));
                    let shown = format!("{}.{}", tenths / 10, tenths % 10);
                    let threshold = machine.threshold;
                    let summary = format!(
                        "这台电脑的数据盘只剩 {free} GB 可用（{shown}%），低于 {threshold}%。请按 ~/.codex/AGENTS.md「总 Router 的职责」：查清是什么占了空间，找到每一项的责任 Router，把清单和建议交给用户；删除要用户点明范围。"
                    );
                    // Without a main Router, the first registered Router hears it, as any notice
                    // without a Router.
                    let router = (machine.main_router)().unwrap_or_default();
                    self.tell_in(tx, &router, NOTICE_KEY, summary).await?;
                }
                Ok(())
            })
            .await?;
        match verdict {
            DiskVerdict::Notify => self.log_machine_event("disk_low", &next.machine),
            DiskVerdict::Rearm => self.log_machine_event("disk_rearmed", &next.machine),
            DiskVerdict::Quiet => {}
        }
        Ok(())
    }
}
