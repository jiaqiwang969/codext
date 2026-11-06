//! Watches the selected account's auth file so the running TUI can reload auth
//! when it is replaced by an external account switcher.

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::mpsc;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::SystemTime;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;

const POLL_INTERVAL: Duration = Duration::from_millis(200);
const DEBOUNCE_DELAY: Duration = Duration::from_millis(200);

pub(crate) struct AuthWatch {
    stop_tx: Option<mpsc::Sender<()>>,
    auth_path: Arc<RwLock<PathBuf>>,
    join_handle: Option<JoinHandle<()>>,
}

impl AuthWatch {
    pub(crate) fn start(codex_home: &Path, app_event_tx: AppEventSender) -> Self {
        let (stop_tx, stop_rx) = mpsc::channel();
        let auth_path = Arc::new(RwLock::new(auth_path_for_profile(
            codex_home,
            std::env::var("CODEX_ACCOUNT_PROFILE").ok().as_deref(),
        )));
        let watch_path = Arc::clone(&auth_path);
        let join_handle = thread::spawn(move || watch_auth_file(watch_path, app_event_tx, stop_rx));
        Self {
            stop_tx: Some(stop_tx),
            auth_path,
            join_handle: Some(join_handle),
        }
    }

    pub(crate) fn set_auth_profile(&self, codex_home: &Path, profile: Option<&str>) {
        let path = auth_path_for_profile(codex_home, profile);
        if let Ok(mut current) = self.auth_path.write() {
            *current = path;
        }
    }
}

impl Drop for AuthWatch {
    fn drop(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AuthFileState {
    is_file: bool,
    len: u64,
    modified: Option<SystemTime>,
}

fn watch_auth_file(
    auth_path: Arc<RwLock<PathBuf>>,
    app_event_tx: AppEventSender,
    stop_rx: mpsc::Receiver<()>,
) {
    let mut current_path = current_auth_path(&auth_path);
    let mut previous = auth_file_state(&current_path);
    loop {
        match stop_rx.recv_timeout(POLL_INTERVAL) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let next_path = current_auth_path(&auth_path);
        if next_path != current_path {
            current_path = next_path;
            previous = auth_file_state(&current_path);
            continue;
        }
        let mut next = auth_file_state(&current_path);
        if next == previous {
            continue;
        }

        // Wait for a quiet period so atomic replacements and partial writes settle first.
        let mut path_changed = false;
        loop {
            match stop_rx.recv_timeout(DEBOUNCE_DELAY) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let settled_path = current_auth_path(&auth_path);
                    if settled_path != current_path {
                        current_path = settled_path;
                        previous = auth_file_state(&current_path);
                        path_changed = true;
                        break;
                    }
                    let settled = auth_file_state(&current_path);
                    if settled == next {
                        break;
                    }
                    next = settled;
                }
            }
        }
        if path_changed {
            continue;
        }
        previous = next;
        app_event_tx.send(AppEvent::AuthFileChanged);
    }
}

fn current_auth_path(auth_path: &Arc<RwLock<PathBuf>>) -> PathBuf {
    auth_path
        .read()
        .map(|path| path.clone())
        .unwrap_or_default()
}

fn auth_path_for_profile(codex_home: &Path, profile: Option<&str>) -> PathBuf {
    if let Some(profile) = profile.filter(|profile| is_safe_profile_name(profile)) {
        return codex_home.join("accounts").join(profile).join("auth.json");
    }
    codex_home.join("auth.json")
}

fn is_safe_profile_name(profile: &str) -> bool {
    !profile.is_empty()
        && Path::new(profile)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        && Path::new(profile).components().count() == 1
}

fn auth_file_state(auth_path: &Path) -> AuthFileState {
    match std::fs::metadata(auth_path) {
        Ok(metadata) if metadata.is_file() => AuthFileState {
            is_file: true,
            len: metadata.len(),
            modified: metadata.modified().ok(),
        },
        _ => AuthFileState {
            is_file: false,
            len: 0,
            modified: None,
        },
    }
}
