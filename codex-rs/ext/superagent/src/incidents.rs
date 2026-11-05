//! Model-side incidents: capacity, rate and usage limits. codex retries them inside the turn
//! and writes nothing to the rollout, so the host reports every stream or turn error here and
//! the engine keeps its own record: one JSON line per incident in `<dir>/incidents.jsonl`, a
//! warn log line, and the last few in memory for status.

use std::collections::VecDeque;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;

use serde::Deserialize;
use serde::Serialize;

static SINK: OnceLock<IncidentLog> = OnceLock::new();
/// Told about every incident recorded, on the recording task: keep it quick.
pub type Listener = Arc<dyn Fn(&Incident) + Send + Sync>;
static LISTENER: OnceLock<Listener> = OnceLock::new();

/// Registers the one listener (the executor's capacity policy); later calls are ignored.
pub fn subscribe(listener: Listener) {
    let _ = LISTENER.set(listener);
}
const RECENT: usize = 100;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Incident {
    /// RFC 3339, UTC.
    pub at: String,
    pub thread: String,
    pub turn: Option<String>,
    /// `capacity`, `rate_limit` or `usage_limit`.
    pub kind: String,
    pub message: String,
}

struct IncidentLog {
    file: PathBuf,
    recent: Mutex<VecDeque<Incident>>,
}

/// Where incidents go; the service installs it once at start (`<dir>/incidents.jsonl`).
pub fn install(dir: &Path) {
    let _ = SINK.set(IncidentLog {
        file: dir.join("incidents.jsonl"),
        recent: Mutex::new(VecDeque::new()),
    });
}

/// The incident kind a model error belongs to; `None` for anything else.
pub fn classify(message: &str, details: Option<&str>) -> Option<&'static str> {
    let text = format!("{message} {}", details.unwrap_or_default()).to_lowercase();
    if text.contains("at capacity") || text.contains("capacity unavailable") {
        Some("capacity")
    } else if text.contains("usage limit") || text.contains("usage_limit") {
        Some("usage_limit")
    } else if text.contains("rate limit")
        || text.contains("too many requests")
        || text.contains(" 429")
    {
        Some("rate_limit")
    } else {
        None
    }
}

/// The host reports a stream or turn error of a thread; capacity and limit conditions are
/// recorded, everything else is ignored.
pub fn note(thread: &str, turn: Option<&str>, message: &str, details: Option<&str>) {
    let Some(log) = SINK.get() else {
        return;
    };
    let Some(kind) = classify(message, details) else {
        return;
    };
    let message = match details {
        Some(details) if !details.is_empty() && details != message => {
            format!("{message}; {details}")
        }
        _ => message.to_owned(),
    };
    tracing::warn!(
        thread,
        turn = turn.unwrap_or(""),
        kind,
        "superagent: model {kind}: {message}"
    );
    let incident = Incident {
        at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        thread: thread.to_owned(),
        turn: turn.map(str::to_owned),
        kind: kind.to_owned(),
        message,
    };
    if let Ok(line) = serde_json::to_string(&incident) {
        if let Some(parent) = log.file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log.file)
        {
            let _ = writeln!(file, "{line}");
        }
    }
    if let Some(listener) = LISTENER.get() {
        listener(&incident);
    }
    let mut recent = log.recent.lock().unwrap_or_else(PoisonError::into_inner);
    if recent.len() >= RECENT {
        recent.pop_front();
    }
    recent.push_back(incident);
}

/// The incidents this process recorded, oldest first (at most the last 100).
pub fn recent() -> Vec<Incident> {
    SINK.get()
        .map(|log| {
            log.recent
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

