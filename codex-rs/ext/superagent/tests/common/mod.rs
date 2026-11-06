#![allow(dead_code)]

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_state::SqliteConfig;
use codex_superagent_extension::Store;
use codex_superagent_extension::core::ActorRef;
use codex_superagent_extension::core::Assignment;
use codex_superagent_extension::core::AssignmentState;
use codex_superagent_extension::core::Decision;
use codex_superagent_extension::core::DecisionKind;
use codex_superagent_extension::core::Ended;
use codex_superagent_extension::core::Mandate;
use codex_superagent_extension::core::MandateBasis;
use codex_superagent_extension::core::Origin;
use codex_superagent_extension::core::Strength;
use codex_superagent_extension::core::Thread;
use codex_superagent_extension::core::ThreadRef;
use codex_superagent_extension::core::Ticket;
use codex_superagent_extension::core::TicketEndBy;
use codex_superagent_extension::core::TicketOutcome;
use codex_superagent_extension::core::Trigger;
use codex_superagent_extension::effects::Clock;
use codex_utils_absolute_path::test_support::PathExt;
use tempfile::TempDir;
use tokio::sync::watch;

/// A fresh `superagent_1.sqlite` in a temporary directory, opened through the state crate.
pub async fn open_store() -> anyhow::Result<(TempDir, Store)> {
    let dir = TempDir::new()?;
    let sqlite = SqliteConfig::new_for_testing(dir.path().abs());
    let pool = codex_state::open_superagent_db(&sqlite).await?;
    Ok((dir, Store::new(pool)))
}

/// A clock the test moves by hand, in milliseconds.
#[derive(Clone)]
pub struct FakeClock(Arc<AtomicI64>);

impl FakeClock {
    pub fn at(ms: i64) -> Self {
        Self(Arc::new(AtomicI64::new(ms)))
    }

    pub fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }

    pub fn clock(&self) -> Clock {
        let inner = Arc::clone(&self.0);
        Arc::new(move || inner.load(Ordering::SeqCst))
    }
}

/// 2026-09-09T00:00:00Z in milliseconds.
pub const T0: i64 = 1_788_912_000_000;

/// Something a handler waits for until the test lets it go.
#[derive(Clone)]
pub struct Hold(watch::Sender<bool>);

impl Hold {
    pub fn new() -> Self {
        let (tx, _) = watch::channel(false);
        Self(tx)
    }

    pub fn resolve(&self) {
        self.0.send_replace(true);
    }

    pub fn wait(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut rx = self.0.subscribe();
        async move {
            let _ = rx.wait_for(|released| *released).await;
        }
    }
}

/// Polls `condition` until it holds, or fails after five seconds.
pub async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let started = std::time::Instant::now();
    while !condition() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

pub async fn wait_until_async<F, Fut>(what: &str, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let started = std::time::Instant::now();
    while !condition().await {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

pub fn ticket(id: &str, ended: bool, states: &[AssignmentState], undelivered: &[&str]) -> Ticket {
    Ticket {
        id: id.to_owned(),
        title: id.to_owned(),
        decision_id: String::new(),
        mandate: Mandate {
            strength: Strength::Explicit,
            policy_version: String::new(),
            basis: MandateBasis::via("listed"),
            actor: ActorRef {
                source: "x".into(),
                id: String::new(),
            },
            scope: String::new(),
        },
        channels: Vec::new(),
        ended: ended.then(|| Ended {
            at: String::new(),
            by: TicketEndBy::Executor,
            outcome: TicketOutcome::Closed,
            reason: Some("closed".into()),
        }),
        attention: None,
        assignments: states
            .iter()
            .enumerate()
            .map(|(index, state)| {
                let mut assignment = Assignment::new(&format!("{id}-{index}"), "");
                assignment.state = *state;
                assignment
            })
            .collect(),
        inputs: Vec::new(),
        undelivered: undelivered.iter().map(|s| (*s).to_owned()).collect(),
        follows: None,
        answers: None,
        created_at: String::new(),
        updated_at: String::new(),
        last_activity: None,
        origin: Origin::default(),
        execution: None,
        delivery: None,
    }
}

pub fn thread(id: &str, source: &str) -> Thread {
    Thread::new(
        ThreadRef {
            source: source.to_owned(),
            id: id.to_owned(),
        },
        "",
    )
}

pub fn trigger(id: &str, source: &str, threads: &[(&str, &str)], at: &str, text: &str) -> Trigger {
    Trigger {
        id: id.to_owned(),
        source: source.to_owned(),
        threads: threads
            .iter()
            .map(|(source, id)| ThreadRef {
                source: (*source).to_owned(),
                id: (*id).to_owned(),
            })
            .collect(),
        actor: ActorRef {
            source: source.to_owned(),
            id: "u".into(),
        },
        human: true,
        addressed: None,
        at: at.to_owned(),
        text: text.to_owned(),
        channel: None,
        url: None,
        meta: serde_json::Map::new(),
    }
}

pub fn decision(id: &str, thread_key: &str) -> Decision {
    Decision {
        id: id.to_owned(),
        eval_id: String::new(),
        thread_key: thread_key.to_owned(),
        trigger_ids: Vec::new(),
        policy_version: String::new(),
        at: id.to_owned(),
        kind: DecisionKind::Ignore,
        reason: Vec::new(),
        strength: Strength::Explicit,
        ticket_id: None,
        mandate: None,
        audit: None,
        dismissed: None,
    }
}

pub fn ids<T, F: Fn(&T) -> &str>(items: &[T], id: F) -> Vec<String> {
    items.iter().map(|item| id(item).to_owned()).collect()
}

pub fn sorted(mut items: Vec<String>) -> Vec<String> {
    items.sort();
    items
}
