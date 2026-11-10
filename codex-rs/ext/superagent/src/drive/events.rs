//! The drives' events: one JSON line in `$CODEX_HOME/superagent/events.jsonl` each time a driven
//! session changes state, step or account, starts or is released, and each time the engine tells
//! the main Router the disk runs low or the free space came back (`machine.rs`). Someone watching
//! the drives follows the file instead of scanning every session's history; the engine never
//! reads it back. The file is cut over to `events.1.jsonl` once it grows past `MAX_BYTES`.
//! `superagent/status` lists every drive (`DriveStatus`) and the machine (`MachineStatus`) in the
//! same shapes.

use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use serde::Serialize;
use serde_json::json;

use super::DriveRecord;
use super::DriveService;
use super::DriveState;
use super::machine::MachineStatus;
use crate::core::now_iso;

const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// One driven session as the events and the engine's status show it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DriveStatus {
    pub thread: String,
    pub title: String,
    pub folder: PathBuf,
    pub step: u32,
    pub max_steps: u32,
    /// `driving`, `reported`, `paused`, `waiting`, `deliverable` or `needs_decision`.
    pub state: &'static str,
    /// What the state says: the summary, the reason, what the session waits for.
    pub detail: String,
    pub account: Option<String>,
    pub router: Option<String>,
    /// What the engine's round noticed.
    pub marks: Vec<String>,
    /// When the state last changed, RFC 3339.
    pub changed: Option<String>,
}

impl DriveStatus {
    pub fn of(record: &DriveRecord) -> Self {
        let (state, detail) = match &record.state {
            DriveState::Driving => ("driving", ""),
            DriveState::Reported { summary } => ("reported", summary.as_str()),
            DriveState::Paused { reason } if record.wait.is_some() => ("waiting", reason.as_str()),
            DriveState::Paused { reason } => ("paused", reason.as_str()),
            DriveState::Deliverable { summary } => ("deliverable", summary.as_str()),
            DriveState::NeedsDecision { reason } => ("needs_decision", reason.as_str()),
        };
        Self {
            thread: record.thread_id.clone(),
            title: record.title.clone(),
            folder: record.project_home.clone(),
            step: record.steps,
            max_steps: record.max_steps,
            state,
            detail: detail.to_owned(),
            account: record.account.clone(),
            router: record.router.clone(),
            marks: record.marks.clone(),
            changed: record.changed.clone(),
        }
    }
}

fn append(path: &Path, line: &str) -> std::io::Result<()> {
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() > MAX_BYTES) {
        std::fs::rename(path, path.with_file_name("events.1.jsonl"))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")
}

impl DriveService {
    /// Writes the event of `record`, `event` naming it when it is not a change of state
    /// (`started`, `released`).
    pub(super) fn log_event(&self, record: &DriveRecord, event: Option<&str>) {
        let Some(path) = &self.inner.events else {
            return;
        };
        let status = DriveStatus::of(record);
        let line = json!({
            "at": now_iso(),
            "event": event.unwrap_or(status.state),
            "drive": status,
        });
        if let Err(err) = append(path, &line.to_string()) {
            tracing::warn!(
                "superagent drive: the event of {} was not written to {}: {err}",
                record.thread_id,
                path.display()
            );
        }
    }

    /// Writes an event about the machine: `event` names it (`disk_low`, `disk_rearmed`), and
    /// `machine` is what the engine read.
    pub(super) fn log_machine_event(&self, event: &str, machine: &MachineStatus) {
        let Some(path) = &self.inner.events else {
            return;
        };
        let line = json!({
            "at": now_iso(),
            "event": event,
            "machine": machine,
        });
        if let Err(err) = append(path, &line.to_string()) {
            tracing::warn!(
                "superagent drive: the event {event} was not written to {}: {err}",
                path.display()
            );
        }
    }
}
