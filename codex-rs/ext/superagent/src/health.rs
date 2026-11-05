//! Component health registry with one writer per entry (prototype `src/infra/health.ts`).

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthState {
    Starting,
    Ready,
    Paused,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Health {
    pub state: HealthState,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
}

impl Health {
    pub fn ready(detail: impl Into<String>, at: Option<String>) -> Self {
        Self {
            state: HealthState::Ready,
            detail: detail.into(),
            at,
        }
    }

    pub fn error(detail: impl Into<String>, at: Option<String>) -> Self {
        Self {
            state: HealthState::Error,
            detail: detail.into(),
            at,
        }
    }
}

#[derive(Default)]
struct Inner {
    entries: BTreeMap<String, Health>,
    claimed: HashSet<String>,
}

/// Central registry of component health entries, enforcing single-writer ownership per name.
#[derive(Clone, Default)]
pub struct HealthBoard {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug, thiserror::Error)]
#[error("The health entry {0} is reported by another part")]
pub struct AlreadyClaimed(pub String);

impl HealthBoard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claims an entry; a second claim of the same name is refused until the first is released.
    pub fn claim(
        &self,
        name: &str,
        initial: Option<Health>,
    ) -> Result<HealthEntry, AlreadyClaimed> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if !inner.claimed.insert(name.to_owned()) {
            return Err(AlreadyClaimed(name.to_owned()));
        }
        if let Some(health) = initial {
            inner.entries.insert(name.to_owned(), health);
        }
        Ok(HealthEntry {
            board: Arc::clone(&self.inner),
            name: name.to_owned(),
            held: Arc::new(AtomicBool::new(true)),
        })
    }

    pub fn get(&self, name: &str) -> Option<Health> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .get(name)
            .cloned()
    }

    pub fn all(&self) -> BTreeMap<String, Health> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .clone()
    }
}

/// One part's handle on its health entry; `set` is a no-op once released.
#[derive(Clone)]
pub struct HealthEntry {
    board: Arc<Mutex<Inner>>,
    name: String,
    held: Arc<AtomicBool>,
}

impl HealthEntry {
    pub fn get(&self) -> Option<Health> {
        self.board
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .get(&self.name)
            .cloned()
    }

    pub fn set(&self, health: Health) {
        if !self.held.load(Ordering::SeqCst) {
            return;
        }
        self.board
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .insert(self.name.clone(), health);
    }

    /// Unregisters the entry so another part may claim the name.
    pub fn release(&self) {
        if !self.held.swap(false, Ordering::SeqCst) {
            return;
        }
        let mut inner = self.board.lock().unwrap_or_else(PoisonError::into_inner);
        inner.claimed.remove(&self.name);
        inner.entries.remove(&self.name);
    }
}

