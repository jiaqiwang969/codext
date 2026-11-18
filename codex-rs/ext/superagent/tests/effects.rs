//! The durable effect queue: coalescing, per-kind limits, retries with backoff, health and shutdown
//! (ported from the prototype's `tests/effects.spec.ts`).

mod common;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use codex_superagent_extension::EffectError;
use codex_superagent_extension::Effects;
use codex_superagent_extension::HandlerOptions;
use codex_superagent_extension::HealthBoard;
use codex_superagent_extension::HealthState;
use codex_superagent_extension::Store;
use codex_superagent_extension::effects::EffectRun;
use common::FakeClock;
use common::Hold;
use common::T0;
use common::open_store;
use common::wait_until;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

struct Setup {
    _dir: TempDir,
    store: Store,
    clock: FakeClock,
    board: HealthBoard,
    effects: Effects,
}

async fn setup() -> anyhow::Result<Setup> {
    let (dir, store) = open_store().await?;
    let clock = FakeClock::at(T0);
    let board = HealthBoard::new();
    let effects = Effects::new(store.clone(), board.clone(), clock.clock()).await?;
    effects.start();
    Ok(Setup {
        _dir: dir,
        store,
        clock,
        board,
        effects,
    })
}

/// Records every call's payload and key, in order.
#[derive(Clone, Default)]
struct Calls(Arc<Mutex<Vec<(Value, String)>>>);

impl Calls {
    fn push(&self, payload: Value, key: String) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((payload, key));
    }

    fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    fn keys(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(_, k)| k.clone())
            .collect()
    }

    fn texts(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(p, _)| p["text"].as_str().unwrap_or_default().to_owned())
            .collect()
    }
}

fn recorder(calls: &Calls) -> EffectRun {
    let calls = calls.clone();
    Arc::new(move |payload, key| {
        calls.push(payload, key);
        Box::pin(async { Ok(()) })
    })
}

fn relay(text: &str) -> Value {
    json!({ "text": text })
}

fn launch(key: &str) -> Value {
    json!({ "ticketId": key, "assignmentId": key })
}

#[tokio::test]
async fn runs_an_effect_queued_in_a_committed_transaction_and_none_from_a_rolled_back_one()
-> anyhow::Result<()> {
    let s = setup().await?;
    let calls = Calls::default();
    s.effects
        .handle("relay", recorder(&calls), HandlerOptions::default())
        .await?;
    s.store
        .atomic(async |tx| s.effects.enqueue(tx, "relay", "kept", &relay("a")).await)
        .await?;
    let rolled_back: anyhow::Result<()> = s
        .store
        .atomic(async |tx| {
            s.effects
                .enqueue(tx, "relay", "dropped", &relay("b"))
                .await?;
            anyhow::bail!("rolled back")
        })
        .await;
    assert_eq!(
        rolled_back.map_err(|e| e.to_string()),
        Err("rolled back".to_owned())
    );
    s.effects.idle(None).await;
    assert_eq!(calls.keys(), vec!["kept"]);
    assert_eq!(calls.texts(), vec!["a"]);
    assert!(!s.effects.pending("relay", None).await?);
    Ok(())
}

#[tokio::test]
async fn keeps_one_effect_per_key_with_the_latest_payload_and_runs_it_again_after_a_new_payload()
-> anyhow::Result<()> {
    let s = setup().await?;
    let calls = Calls::default();
    let hold = Hold::new();
    let run: EffectRun = {
        let calls = calls.clone();
        let hold = hold.clone();
        Arc::new(move |payload: Value, key| {
            calls.push(payload.clone(), key);
            let wait = hold.wait();
            Box::pin(async move {
                if payload["text"] == "first" {
                    wait.await;
                }
                Ok(())
            })
        })
    };
    s.effects
        .handle("relay", run, HandlerOptions::default())
        .await?;
    s.effects.enqueue_now("relay", "k", &relay("first")).await?;
    wait_until("first call", || calls.len() == 1).await;
    s.effects
        .enqueue_now("relay", "k", &relay("second"))
        .await?;
    s.effects.enqueue_now("relay", "k", &relay("third")).await?;
    hold.resolve();
    s.effects.idle(None).await;
    assert_eq!(calls.texts(), vec!["first", "third"]);
    Ok(())
}

#[tokio::test]
async fn runs_a_kind_oldest_first_up_to_its_limit_and_holds_it_while_the_limit_is_zero()
-> anyhow::Result<()> {
    let s = setup().await?;
    let calls = Calls::default();
    let holds: Arc<Mutex<std::collections::HashMap<String, Hold>>> = Arc::default();
    let limit = Arc::new(AtomicUsize::new(2));
    let run: EffectRun = {
        let calls = calls.clone();
        let holds = Arc::clone(&holds);
        Arc::new(move |payload, key: String| {
            calls.push(payload, key.clone());
            let hold = Hold::new();
            holds
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key, hold.clone());
            let wait = hold.wait();
            Box::pin(async move {
                wait.await;
                Ok(())
            })
        })
    };
    let limit_for_handler = Arc::clone(&limit);
    s.effects
        .handle(
            "launch",
            run,
            HandlerOptions::default().limit(move || Ok(limit_for_handler.load(Ordering::SeqCst))),
        )
        .await?;
    for key in ["a", "b", "c"] {
        s.effects.enqueue_now("launch", key, &launch(key)).await?;
    }
    wait_until("a and b", || calls.keys() == vec!["a", "b"]).await;
    limit.store(0, Ordering::SeqCst);
    let release = |key: &str| {
        if let Some(hold) = holds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
        {
            hold.resolve();
        }
    };
    release("a");
    let effects = s.effects.clone();
    common::wait_until_async("a done", || {
        let effects = effects.clone();
        async move { !effects.pending("launch", Some("a")).await.unwrap_or(true) }
    })
    .await;
    s.effects.kick();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(calls.len(), 2);
    limit.store(2, Ordering::SeqCst);
    release("b");
    wait_until("c started", || calls.keys() == vec!["a", "b", "c"]).await;
    release("c");
    s.effects.idle(None).await;
    Ok(())
}

#[tokio::test]
async fn keeps_an_effects_place_in_its_queue_when_queued_again_with_a_new_payload()
-> anyhow::Result<()> {
    let s = setup().await?;
    let calls = Calls::default();
    let room = Arc::new(AtomicUsize::new(0));
    let room_for_handler = Arc::clone(&room);
    s.effects
        .handle(
            "launch",
            recorder(&calls),
            HandlerOptions::default().limit(move || Ok(room_for_handler.load(Ordering::SeqCst))),
        )
        .await?;
    s.store
        .atomic(async |tx| {
            s.effects
                .enqueue(
                    tx,
                    "launch",
                    "first",
                    &json!({ "ticketId": "t1", "assignmentId": "a1" }),
                )
                .await?;
            s.effects
                .enqueue(
                    tx,
                    "launch",
                    "second",
                    &json!({ "ticketId": "t2", "assignmentId": "a2" }),
                )
                .await?;
            s.effects
                .enqueue(
                    tx,
                    "launch",
                    "first",
                    &json!({ "ticketId": "t1", "assignmentId": "a1" }),
                )
                .await
        })
        .await?;
    s.effects.idle(None).await;
    room.store(1, Ordering::SeqCst);
    s.effects.idle(None).await;
    assert_eq!(calls.keys(), vec!["first", "second"]);
    Ok(())
}

#[tokio::test]
async fn retries_a_failed_effect_with_a_doubling_delay_and_reports_the_kind_in_error_after_three_failures()
-> anyhow::Result<()> {
    let s = setup().await?;
    let fail = Arc::new(AtomicBool::new(true));
    let calls = Arc::new(AtomicUsize::new(0));
    let run: EffectRun = {
        let fail = Arc::clone(&fail);
        let calls = Arc::clone(&calls);
        Arc::new(move |_, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            let fail = fail.load(Ordering::SeqCst);
            Box::pin(async move {
                if fail {
                    Err(EffectError::Failed("session busy".into()))
                } else {
                    Ok(())
                }
            })
        })
    };
    s.effects
        .handle("notice", run, HandlerOptions::default().retry(1000, 3000))
        .await?;
    s.effects
        .enqueue_now("notice", "prompts", &json!({ "value": { "prompt_config_saved": 2 }, "summary": "Prompt configuration saved" }))
        .await?;
    s.effects.idle(None).await;
    let health = s
        .board
        .get("notice")
        .ok_or_else(|| anyhow::anyhow!("no health"))?;
    assert_eq!(health.state, HealthState::Ready);
    assert_eq!(health.detail, "1 waiting since 2026-09-09T00:00:00.000Z");
    for wait in [1000, 2000, 3000, 3000] {
        s.clock.advance(wait - 1);
        s.effects.idle(None).await;
        let before = calls.load(Ordering::SeqCst);
        s.clock.advance(1);
        s.effects.idle(None).await;
        assert_eq!(calls.load(Ordering::SeqCst), before + 1);
        if wait == 2000 {
            let health = s
                .board
                .get("notice")
                .ok_or_else(|| anyhow::anyhow!("no health"))?;
            assert_eq!(health.state, HealthState::Error);
            assert!(
                health.detail.contains(
                    "1 waiting since 2026-09-09T00:00:00.000Z; the last failure: session busy"
                ),
                "{}",
                health.detail
            );
        }
    }
    fail.store(false, Ordering::SeqCst);
    s.clock.advance(3000);
    s.effects.idle(None).await;
    let health = s
        .board
        .get("notice")
        .ok_or_else(|| anyhow::anyhow!("no health"))?;
    assert_eq!(
        (health.state, health.detail.as_str()),
        (HealthState::Ready, "Nothing waiting")
    );
    assert!(!s.effects.pending("notice", None).await?);
    Ok(())
}

#[tokio::test]
async fn starts_nothing_once_drained_keeps_what_waits_and_runs_it_after_the_next_start()
-> anyhow::Result<()> {
    let s = setup().await?;
    let hold = Hold::new();
    let run: EffectRun = {
        let hold = hold.clone();
        Arc::new(move |_, _| {
            let wait = hold.wait();
            Box::pin(async move {
                wait.await;
                Err(EffectError::Failed("aborted".into()))
            })
        })
    };
    s.effects
        .handle("relay", run, HandlerOptions::default())
        .await?;
    s.effects.enqueue_now("relay", "cut", &relay("a")).await?;
    let effects = s.effects.clone();
    common::wait_until_async("cut pending", || {
        let effects = effects.clone();
        async move { effects.pending("relay", Some("cut")).await.unwrap_or(false) }
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let drained = tokio::spawn({
        let effects = s.effects.clone();
        async move { effects.drain().await }
    });
    s.effects.enqueue_now("relay", "later", &relay("b")).await?;
    hold.resolve();
    drained.await?;
    let board = HealthBoard::new();
    let next = Effects::new(s.store.clone(), board.clone(), s.clock.clock()).await?;
    next.start();
    let calls = Calls::default();
    next.handle("relay", recorder(&calls), HandlerOptions::default())
        .await?;
    next.idle(None).await;
    let pending_later = next.pending("relay", Some("later")).await?;
    assert_eq!(
        calls.keys(),
        vec!["cut", "later"],
        "later still pending: {pending_later}; health: {:?}",
        board.get("relay")
    );
    Ok(())
}

#[tokio::test]
async fn lets_a_handler_defer_an_effect_without_a_failure_until_after_its_first_delay()
-> anyhow::Result<()> {
    let s = setup().await?;
    let later = Arc::new(AtomicBool::new(true));
    let calls = Arc::new(AtomicUsize::new(0));
    let run: EffectRun = {
        let later = Arc::clone(&later);
        let calls = Arc::clone(&calls);
        Arc::new(move |_, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            let later = later.load(Ordering::SeqCst);
            Box::pin(async move {
                if later {
                    Err(EffectError::Later("paused".into()))
                } else {
                    Ok(())
                }
            })
        })
    };
    s.effects
        .handle("launch", run, HandlerOptions::default().retry(5000, 60_000))
        .await?;
    s.effects.enqueue_now("launch", "a", &launch("a")).await?;
    s.effects.idle(None).await;
    let health = s
        .board
        .get("launch")
        .ok_or_else(|| anyhow::anyhow!("no health"))?;
    assert_eq!(
        (health.state, health.detail.as_str()),
        (
            HealthState::Ready,
            "1 waiting since 2026-09-09T00:00:00.000Z"
        )
    );
    later.store(false, Ordering::SeqCst);
    s.clock.advance(4999);
    s.effects.idle(None).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    s.clock.advance(1);
    s.effects.idle(None).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(!s.effects.pending("launch", None).await?);
    Ok(())
}

#[tokio::test]
async fn runs_an_ordered_kind_one_at_a_time_in_order_and_holds_later_effects_while_an_earlier_one_fails()
-> anyhow::Result<()> {
    let s = setup().await?;
    let failing = Arc::new(Mutex::new(std::collections::HashSet::from([
        "first".to_owned()
    ])));
    let calls = Calls::default();
    let run: EffectRun = {
        let failing = Arc::clone(&failing);
        let calls = calls.clone();
        Arc::new(move |payload, key: String| {
            calls.push(payload, key.clone());
            let fails = failing
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains(&key);
            Box::pin(async move {
                if fails {
                    Err(EffectError::Failed("offline".into()))
                } else {
                    Ok(())
                }
            })
        })
    };
    s.effects
        .handle(
            "relay",
            run,
            HandlerOptions::default()
                .ordered()
                .limit(|| Ok(4))
                .retry(1000, 1000),
        )
        .await?;
    s.effects.enqueue_now("relay", "first", &relay("a")).await?;
    s.effects
        .enqueue_now("relay", "second", &relay("b"))
        .await?;
    s.effects.idle(None).await;
    s.clock.advance(1000);
    s.effects.idle(None).await;
    assert_eq!(calls.keys(), vec!["first", "first"]);
    failing
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    s.clock.advance(1000);
    s.effects.idle(None).await;
    assert_eq!(calls.keys(), vec!["first", "first", "first", "second"]);
    Ok(())
}

#[tokio::test]
async fn keeps_a_quiet_kinds_failures_out_of_its_health_and_asks_no_limit_while_nothing_is_due()
-> anyhow::Result<()> {
    let s = setup().await?;
    let limit_calls = Arc::new(AtomicUsize::new(0));
    let run: EffectRun =
        Arc::new(|_, _| Box::pin(async { Err(EffectError::Failed("Session gone".into())) }));
    let limit_calls_for_handler = Arc::clone(&limit_calls);
    s.effects
        .handle(
            "delivery",
            run,
            HandlerOptions::default()
                .quiet()
                .retry(1000, 1000)
                .limit(move || {
                    limit_calls_for_handler.fetch_add(1, Ordering::SeqCst);
                    Ok(4)
                }),
        )
        .await?;
    s.effects.kick();
    s.effects.idle(None).await;
    assert_eq!(limit_calls.load(Ordering::SeqCst), 0);
    s.effects
        .enqueue_now("delivery", "t", &json!({ "ticketId": "t" }))
        .await?;
    for _ in 0..4 {
        s.effects.idle(None).await;
        s.clock.advance(1000);
    }
    let health = s
        .board
        .get("delivery")
        .ok_or_else(|| anyhow::anyhow!("no health"))?;
    assert_eq!(
        (health.state, health.detail.as_str()),
        (
            HealthState::Ready,
            "1 waiting since 2026-09-09T00:00:00.000Z"
        )
    );
    Ok(())
}

#[tokio::test]
async fn keeps_an_effect_cancelled_and_queued_again_while_its_first_run_goes_on()
-> anyhow::Result<()> {
    let s = setup().await?;
    let calls = Calls::default();
    let hold = Hold::new();
    let run: EffectRun = {
        let calls = calls.clone();
        let hold = hold.clone();
        Arc::new(move |payload, key| {
            let first = calls.len() == 0;
            calls.push(payload, key);
            let wait = hold.wait();
            Box::pin(async move {
                if first {
                    wait.await;
                }
                Ok(())
            })
        })
    };
    s.effects
        .handle("relay", run, HandlerOptions::default())
        .await?;
    s.effects.enqueue_now("relay", "k", &relay("first")).await?;
    wait_until("first call", || calls.len() == 1).await;
    s.store
        .atomic(async |tx| {
            s.effects.cancel(tx, "relay", "k").await?;
            s.effects.enqueue(tx, "relay", "k", &relay("second")).await
        })
        .await?;
    hold.resolve();
    s.effects.idle(None).await;
    assert_eq!(calls.texts(), vec!["first", "second"]);
    assert!(!s.effects.pending("relay", None).await?);
    Ok(())
}

#[tokio::test]
async fn queues_an_effect_after_a_restart_behind_the_ones_that_waited_for_an_ordered_kind()
-> anyhow::Result<()> {
    let s = setup().await?;
    let hold = Hold::new();
    let run: EffectRun = {
        let hold = hold.clone();
        Arc::new(move |_, _| {
            let wait = hold.wait();
            Box::pin(async move {
                wait.await;
                Err(EffectError::Failed("aborted".into()))
            })
        })
    };
    s.effects
        .handle("relay", run, HandlerOptions::default().ordered())
        .await?;
    s.effects.enqueue_now("relay", "first", &relay("a")).await?;
    s.effects
        .enqueue_now("relay", "second", &relay("b"))
        .await?;
    let effects = s.effects.clone();
    common::wait_until_async("first pending", || {
        let effects = effects.clone();
        async move {
            effects
                .pending("relay", Some("first"))
                .await
                .unwrap_or(false)
        }
    })
    .await;
    let drained = tokio::spawn({
        let effects = s.effects.clone();
        async move { effects.drain().await }
    });
    hold.resolve();
    drained.await?;
    // Sequence numbers continue across restarts, so what is queued now runs after what waited.
    let next = Effects::new(s.store.clone(), HealthBoard::new(), s.clock.clock()).await?;
    next.enqueue_now("relay", "third", &relay("c")).await?;
    next.start();
    let calls = Calls::default();
    next.handle(
        "relay",
        recorder(&calls),
        HandlerOptions::default().ordered(),
    )
    .await?;
    next.idle(None).await;
    assert_eq!(calls.keys(), vec!["first", "second", "third"]);
    Ok(())
}

#[tokio::test]
async fn reports_a_failure_of_the_queue_itself_under_its_kind_until_a_dispatch_succeeds_again()
-> anyhow::Result<()> {
    let s = setup().await?;
    let broken = Arc::new(AtomicBool::new(true));
    let broken_for_handler = Arc::clone(&broken);
    let run: EffectRun = Arc::new(|_, _| Box::pin(async { Ok(()) }));
    s.effects
        .handle(
            "relay",
            run,
            HandlerOptions::default().limit(move || {
                if broken_for_handler.load(Ordering::SeqCst) {
                    Err("database is locked".to_owned())
                } else {
                    Ok(1)
                }
            }),
        )
        .await?;
    s.effects.enqueue_now("relay", "k", &relay("a")).await?;
    s.effects.idle(None).await;
    let health = s
        .board
        .get("relay")
        .ok_or_else(|| anyhow::anyhow!("no health"))?;
    assert_eq!(
        (health.state, health.detail.as_str()),
        (
            HealthState::Error,
            "The effect queue failed: database is locked"
        )
    );
    broken.store(false, Ordering::SeqCst);
    s.effects.kick();
    s.effects.idle(None).await;
    let health = s
        .board
        .get("relay")
        .ok_or_else(|| anyhow::anyhow!("no health"))?;
    assert_eq!(
        (health.state, health.detail.as_str()),
        (HealthState::Ready, "Nothing waiting")
    );
    Ok(())
}

#[tokio::test]
async fn refuses_a_second_handler_for_a_kind() -> anyhow::Result<()> {
    let s = setup().await?;
    let run: EffectRun = Arc::new(|_, _| Box::pin(async { Ok(()) }));
    s.effects
        .handle("relay", Arc::clone(&run), HandlerOptions::default())
        .await?;
    let second = s
        .effects
        .handle("relay", run, HandlerOptions::default())
        .await;
    assert_eq!(
        second.map_err(|e| e.to_string()),
        Err("The relay effects already have a handler".to_owned())
    );
    Ok(())
}
