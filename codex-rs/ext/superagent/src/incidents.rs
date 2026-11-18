//! Model-side incidents: capacity, rate and usage limits. codex retries them inside the turn
//! and writes nothing to the rollout, so the host reports every stream or turn error here and
//! the engine keeps its own record of the threads it drives: one JSON line per incident in
//! `<dir>/incidents.jsonl`, a warn log line, and the last few in memory for status.
//!
//! Several parts listen: the executor's capacity policy (worker fallback model) and the drive's
//! Router rescue each subscribe, and each limits the record to the threads it cares about.

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
static LISTENERS: Mutex<Vec<Listener>> = Mutex::new(Vec::new());

/// Adds a listener (the executor's capacity policy, the drive's Router rescue); every listener
/// is told about every incident recorded, after it is in `recent`.
pub fn subscribe(listener: Listener) {
    LISTENERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(listener);
}

/// Which threads' incidents are recorded; answered on the reporting task: keep it quick.
pub type Scope = Arc<dyn Fn(&str) -> bool + Send + Sync>;
static SCOPES: Mutex<Vec<Scope>> = Mutex::new(Vec::new());

/// Records the threads `scope` accepts (the executor's: the PM and live worker threads; the
/// drive's: the Routers), besides those an earlier scope accepts. Until a scope is set, every
/// thread is recorded.
pub fn limit_to(scope: Scope) {
    SCOPES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(scope);
}

/// Whether some scope accepts the thread, or none is set.
fn in_scope(thread: &str) -> bool {
    let scopes = SCOPES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    scopes.is_empty() || scopes.iter().any(|scope| scope(thread))
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

/// The host reports a stream or turn error of a thread; capacity and limit conditions of the
/// threads in scope are recorded, everything else is ignored.
pub fn note(thread: &str, turn: Option<&str>, message: &str, details: Option<&str>) {
    let Some(log) = SINK.get() else {
        return;
    };
    let Some(kind) = classify(message, details) else {
        return;
    };
    if !in_scope(thread) {
        return;
    }
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
    {
        let mut recent = log.recent.lock().unwrap_or_else(PoisonError::into_inner);
        if recent.len() >= RECENT {

            recent.pop_front();
        }
        recent.push_back(incident.clone());
    }
    // Outside the lock: a listener counts the recent incidents, this one included.
    let listeners = LISTENERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    for listener in listeners {
        listener(&incident);
    }
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
