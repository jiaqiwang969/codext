//! Only the threads the engine drives reach the incident record.

use std::sync::Arc;

use codex_superagent_extension::incidents;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[test]
fn other_sessions_errors_stay_out_of_the_record() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    incidents::install(dir.path());
    incidents::limit_to(Arc::new(|thread: &str| thread == "worker-thread"));
    for thread in ["worker-thread", "someone-elses-session"] {
        incidents::note(
            thread,
            Some("turn-1"),
            "Selected model is at capacity",
            /*details*/ None,
        );
    }
    incidents::note(
        "worker-thread",
        Some("turn-2"),
        "unrelated failure",
        /*details*/ None,
    );

    let recorded: Vec<(String, String)> = incidents::recent()
        .into_iter()
        .map(|incident| (incident.thread, incident.kind))
        .collect();
    assert_eq!(
        recorded,
        vec![("worker-thread".to_owned(), "capacity".to_owned())]
    );
    let lines = std::fs::read_to_string(dir.path().join("incidents.jsonl"))?;
    assert_eq!(lines.lines().count(), 1);
    Ok(())
}
