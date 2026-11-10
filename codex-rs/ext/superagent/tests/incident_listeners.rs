//! The incident record has several listeners and several scopes: the executor's capacity policy
//! and the drive's Router rescue both hear every incident recorded, and a thread either scope
//! accepts is recorded. Its own test binary, since the record is process-wide.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use codex_superagent_extension::incidents;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

/// What each listener heard: (listener, thread, incidents of that thread recorded by then).
type Heard = Arc<Mutex<Vec<(&'static str, String, usize)>>>;

#[test]
fn every_listener_hears_each_incident_of_a_thread_any_scope_accepts() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    incidents::install(dir.path());
    incidents::limit_to(Arc::new(|thread: &str| thread == "worker-thread"));
    incidents::limit_to(Arc::new(|thread: &str| thread == "router-thread"));
    let heard: Heard = Arc::default();
    for listener in ["capacity policy", "router rescue"] {
        let heard = Arc::clone(&heard);
        incidents::subscribe(Arc::new(move |incident: &incidents::Incident| {
            let recorded = incidents::recent()
                .iter()
                .filter(|recent| recent.thread == incident.thread)
                .count();
            heard.lock().unwrap_or_else(PoisonError::into_inner).push((
                listener,
                incident.thread.clone(),
                recorded,
            ));
        }));
    }
    for thread in ["worker-thread", "router-thread", "someone-elses-session"] {
        incidents::note(
            thread,
            Some("turn-1"),
            "Reconnecting... 1/5",
            Some("Selected model is at capacity"),
        );
    }

    let heard = heard.lock().unwrap_or_else(PoisonError::into_inner).clone();
    assert_eq!(
        heard,
        vec![
            ("capacity policy", "worker-thread".to_owned(), 1),
            ("router rescue", "worker-thread".to_owned(), 1),
            ("capacity policy", "router-thread".to_owned(), 1),
            ("router rescue", "router-thread".to_owned(), 1),
        ]
    );
    let lines = std::fs::read_to_string(dir.path().join("incidents.jsonl"))?;
    assert_eq!(lines.lines().count(), 2);
    Ok(())
}
