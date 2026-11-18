//! The store: transactions, working sets, timelines and key-value state (ported from the
//! prototype's `tests/store.spec.ts`, the SQLite half).

mod common;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use codex_superagent_extension::core::AssignmentState::Ended;
use codex_superagent_extension::core::AssignmentState::Pending;
use codex_superagent_extension::core::AssignmentState::Running;
use codex_superagent_extension::core::Evaluation;
use codex_superagent_extension::core::MandateBasis;
use codex_superagent_extension::core::Strength;
use codex_superagent_extension::core::ThreadRef;
use codex_superagent_extension::core::TriggerDisposition;
use codex_superagent_extension::store::MessageKind;
use codex_superagent_extension::store::OutboundMessage;
use codex_superagent_extension::store::ReplyStatus;
use codex_superagent_extension::store::Settle;
use codex_superagent_extension::store::TriggerFilter;
use common::decision;
use common::ids;
use common::open_store;
use common::sorted;
use common::thread;
use common::ticket;
use common::trigger;
use pretty_assertions::assert_eq;
use serde_json::json;

#[tokio::test]
async fn rolls_back_failed_atomic_work() -> anyhow::Result<()> {
    let (_dir, store) = open_store().await?;
    let result: anyhow::Result<()> = store
        .atomic(async |tx| {
            tx.kv_set("a", &json!(1)).await?;
            anyhow::bail!("boom")
        })
        .await;
    assert_eq!(result.map_err(|e| e.to_string()), Err("boom".to_owned()));
    assert_eq!(store.kv_get::<serde_json::Value>("a").await?, None);
    store
        .atomic(async |tx| tx.kv_set("a", &json!(2)).await)
        .await?;
    assert_eq!(store.kv_get::<i64>("a").await?, Some(2));
    Ok(())
}

#[tokio::test]
async fn finds_triggers_by_thread() -> anyhow::Result<()> {
    let (_dir, store) = open_store().await?;
    store
        .atomic(async |tx| {
            tx.save_trigger(&trigger(
                "a",
                "feishu",
                &[("feishu", "oc_1:om_%_x")],
                "2",
                "later",
            ))
            .await?;
            tx.save_trigger(&trigger(
                "b",
                "feishu",
                &[("feishu", "oc_1:om_%_x"), ("github", "3")],
                "1",
                "earlier",
            ))
            .await?;
            tx.save_trigger(&trigger(
                "c",
                "feishu",
                &[("feishu", "oc_1:om_y_x")],
                "3",
                "other",
            ))
            .await
        })
        .await?;
    let on = |items: Vec<codex_superagent_extension::core::Trigger>| ids(&items, |t| &t.id);
    assert_eq!(
        on(store
            .thread_triggers("feishu:oc_1:om_%_x", 50, None, false)
            .await?),
        vec!["b", "a"]
    );
    assert_eq!(
        on(store.thread_triggers("github:3", 50, None, false).await?),
        vec!["b"]
    );
    // Saving a Trigger again replaces the Threads it is found on.
    let mut b = store
        .trigger("b")
        .await?
        .ok_or_else(|| anyhow::anyhow!("b"))?;
    b.threads = vec![ThreadRef {
        source: "github".into(),
        id: "3".into(),
    }];
    store.atomic(async |tx| tx.save_trigger(&b).await).await?;
    assert_eq!(
        on(store
            .thread_triggers("feishu:oc_1:om_%_x", 50, None, false)
            .await?),
        vec!["a"]
    );
    assert_eq!(
        on(store.thread_triggers("github:3", 50, None, false).await?),
        vec!["b"]
    );
    assert!(store.subscribed_threads(None).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn reads_the_working_sets_alone_and_follows_each_save() -> anyhow::Result<()> {
    let (_dir, store) = open_store().await?;
    store
        .atomic(async |tx| {
            tx.save_ticket(&ticket("running", false, &[Running], &[]))
                .await?;
            // A retry or a resume adds a pending Assignment after the ones that ended.
            tx.save_ticket(&ticket("queued", false, &[Ended, Pending], &[]))
                .await?;
            // An ended Ticket a person's message reached without resuming it.
            tx.save_ticket(&ticket("waking", true, &[Ended], &["input"]))
                .await?;
            tx.save_ticket(&ticket("done", true, &[Ended], &[])).await?;
            let mut t1 = thread("1", "github");
            t1.pending = vec!["a".into()];
            tx.save_thread(&t1).await?;
            let mut t2 = thread("2", "github");
            t2.subscriptions = vec!["running".into()];
            t2.evaluating = Some(Evaluation {
                id: "e".into(),
                thread_key: "github:2".into(),
                trigger_ids: vec!["b".into()],
                primary: None,
                strength: Strength::Explicit,
                actor: codex_superagent_extension::core::ActorRef {
                    source: "github".into(),
                    id: "alice".into(),
                },
                basis: MandateBasis::via("followup"),
                policy_version: String::new(),
                started_at: String::new(),
                delivered: None,
                deadline: String::new(),
                paused_at: None,
            });
            tx.save_thread(&t2).await?;
            let mut t3 = thread("oc:om", "feishu");
            t3.subscriptions = vec!["done".into()];
            tx.save_thread(&t3).await?;
            tx.save_thread(&thread("3", "github")).await
        })
        .await?;
    assert_eq!(
        sorted(ids(&store.tickets_open().await?, |t| &t.id)),
        vec!["queued", "running"]
    );
    assert_eq!(
        sorted(ids(&store.tickets_active().await?, |t| &t.id)),
        vec!["queued", "running", "waking"]
    );
    assert_eq!(
        sorted(ids(&store.threads_active().await?, |t| &t.key)),
        vec!["github:1", "github:2"]
    );
    store
        .atomic(async |tx| {
            tx.save_ticket(&ticket("running", true, &[Ended], &[]))
                .await?;
            tx.save_thread(&thread("1", "github")).await
        })
        .await?;
    assert_eq!(
        sorted(ids(&store.tickets_open().await?, |t| &t.id)),
        vec!["queued"]
    );
    assert_eq!(
        sorted(ids(&store.tickets_active().await?, |t| &t.id)),
        vec!["queued", "waking"]
    );
    assert_eq!(
        sorted(ids(&store.threads_active().await?, |t| &t.key)),
        vec!["github:2"]
    );
    assert_eq!(
        sorted(ids(&store.subscribed_threads(None).await?, |t| &t.key)),
        vec!["feishu:oc:om", "github:2"]
    );
    assert_eq!(
        sorted(ids(&store.subscribed_threads(Some("github")).await?, |t| {
            &t.key
        })),
        vec!["github:2"]
    );
    Ok(())
}

#[tokio::test]
async fn names_a_thread_by_its_first_human_trigger() -> anyhow::Result<()> {
    let (_dir, store) = open_store().await?;
    store
        .atomic(async |tx| {
            let mut ci = trigger(
                "ci",
                "github",
                &[("github", "5")],
                "1",
                "GitHub PR #5 CI: {}",
            );
            ci.human = false;
            tx.save_trigger(&ci).await?;
            tx.save_trigger(&trigger(
                "person",
                "github",
                &[("github", "5")],
                "2",
                "please look",
            ))
            .await
        })
        .await?;
    assert_eq!(
        store.first_trigger("github:5", false).await?.map(|t| t.id),
        Some("ci".to_owned())
    );
    assert_eq!(
        store.first_trigger("github:5", true).await?.map(|t| t.id),
        Some("person".to_owned())
    );
    Ok(())
}

#[tokio::test]
async fn counts_triggers_per_source_and_lists_them_newest_first() -> anyhow::Result<()> {
    let (_dir, store) = open_store().await?;
    store
        .atomic(async |tx| {
            tx.save_trigger(&trigger(
                "old",
                "feishu",
                &[("feishu", "x")],
                "2026-09-01T00:00:00Z",
                "old",
            ))
            .await?;
            tx.save_trigger(&trigger(
                "kept",
                "feishu",
                &[("feishu", "x")],
                "2026-09-23T00:00:00Z",
                "kept",
            ))
            .await?;
            tx.save_trigger(&trigger(
                "dropped",
                "feishu",
                &[("feishu", "x")],
                "2026-09-23T01:00:00Z",
                "dropped",
            ))
            .await?;
            tx.save_disposition(
                "dropped",
                &TriggerDisposition {
                    filtered: Some(codex_superagent_extension::core::Reason::code(
                        "chat_not_allowed",
                    )),
                    at: String::new(),
                    ..TriggerDisposition::default()
                },
            )
            .await?;
            tx.save_trigger(&trigger(
                "gh",
                "github",
                &[("github", "x")],
                "2026-09-22T00:00:00Z",
                "gh",
            ))
            .await
        })
        .await?;
    let stats = store.trigger_stats("2026-09-22T12:00:00Z").await?;
    assert_eq!(
        (
            stats["feishu"].received,
            stats["feishu"].filtered,
            stats["feishu"].last_at.as_str()
        ),
        (2, 1, "2026-09-23T01:00:00Z")
    );
    assert_eq!(
        (
            stats["github"].received,
            stats["github"].filtered,
            stats["github"].last_at.as_str()
        ),
        (0, 0, "2026-09-22T00:00:00Z")
    );
    let (items, more) = store
        .triggers(
            &TriggerFilter {
                source: Some("feishu".into()),
                filtered: false,
            },
            0,
            2,
        )
        .await?;
    assert_eq!(
        (ids(&items, |t| &t.id), more),
        (vec!["dropped".to_owned(), "kept".to_owned()], true)
    );
    let (items, _) = store
        .triggers(
            &TriggerFilter {
                source: None,
                filtered: true,
            },
            0,
            50,
        )
        .await?;
    assert_eq!(ids(&items, |t| &t.id), vec!["dropped"]);
    assert_eq!(
        ids(&store.recent_triggers(0, 2).await?, |t| &t.id),
        vec!["gh", "dropped"]
    );
    Ok(())
}

#[tokio::test]
async fn sets_and_removes_a_sources_keys_in_a_threads_meta() -> anyhow::Result<()> {
    let (_dir, store) = open_store().await?;
    store
        .atomic(async |tx| {
            let mut t = thread("5", "github");
            t.meta = json!({ "isPr": true, "gone": true })
                .as_object()
                .cloned()
                .unwrap_or_default();
            tx.save_thread(&t).await?;
            let patch = json!({ "gone": null, "title": "Fix" })
                .as_object()
                .cloned()
                .unwrap_or_default();
            tx.patch_thread_meta("github:5", &patch).await?;
            let missing = json!({ "title": "none" })
                .as_object()
                .cloned()
                .unwrap_or_default();
            tx.patch_thread_meta("github:6", &missing).await
        })
        .await?;
    let meta = store
        .thread("github:5")
        .await?
        .map(|t| serde_json::Value::Object(t.meta));
    assert_eq!(meta, Some(json!({ "isPr": true, "title": "Fix" })));
    assert_eq!(store.thread("github:6").await?, None);
    Ok(())
}

#[tokio::test]
async fn runs_what_waits_for_a_commit_once_the_outermost_transaction_commits_and_never_what_rolled_back()
-> anyhow::Result<()> {
    let (_dir, store) = open_store().await?;
    let ran: Arc<Mutex<Vec<&'static str>>> = Arc::default();
    let push = |label: &'static str| {
        let ran = Arc::clone(&ran);
        move || {
            ran.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(label)
        }
    };
    store
        .atomic(async |tx| {
            tx.after_commit(push("outer"));
            tx.atomic(async |tx| {
                tx.after_commit(push("inner"));
                Ok(())
            })
            .await?;
            let rolled_back: anyhow::Result<()> = tx
                .atomic(async |tx| {
                    tx.after_commit(push("rolled back"));
                    anyhow::bail!("boom")
                })
                .await;
            assert_eq!(
                rolled_back.map_err(|e| e.to_string()),
                Err("boom".to_owned())
            );
            // Nothing runs before the outermost transaction commits.
            assert!(
                ran.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .is_empty()
            );
            Ok(())
        })
        .await?;
    assert_eq!(
        *ran.lock().unwrap_or_else(PoisonError::into_inner),
        vec!["outer", "inner"]
    );
    let failed: anyhow::Result<()> = store
        .atomic(async |tx| {
            tx.after_commit(push("failed"));
            anyhow::bail!("boom")
        })
        .await;
    assert!(failed.is_err());
    assert_eq!(
        *ran.lock().unwrap_or_else(PoisonError::into_inner),
        vec!["outer", "inner"]
    );
    Ok(())
}

#[tokio::test]
async fn lists_in_a_fixed_order_and_undoes_a_failed_nested_transaction_alone() -> anyhow::Result<()>
{
    let (_dir, store) = open_store().await?;
    store
        .atomic(async |tx| {
            tx.save_ticket(&ticket("first", false, &[Running], &[]))
                .await?;
            tx.save_ticket(&ticket("second", true, &[Ended], &[]))
                .await?;
            tx.save_ticket(&ticket("third", false, &[Pending], &[]))
                .await?;
            // Saving a ticket again keeps its place.
            let mut first = tx
                .ticket("first")
                .await?
                .ok_or_else(|| anyhow::anyhow!("first"))?;
            first.title = "again".into();
            tx.save_ticket(&first).await?;
            let mut t1 = thread("1", "github");
            t1.pending = vec!["a".into()];
            tx.save_thread(&t1).await?;
            tx.save_thread(&thread("2", "github")).await?;
            let mut t3 = thread("3", "github");
            t3.pending = vec!["b".into()];
            tx.save_thread(&t3).await?;
            tx.save_decision(&decision("d2", "github:1")).await?;
            tx.save_decision(&decision("d1", "github:1")).await?;
            // A failed nested transaction undoes its own writes; the one around it goes on and keeps its own.
            tx.atomic(async |tx| {
                tx.save_ticket(&ticket("outer", false, &[], &[])).await?;
                let inner: anyhow::Result<()> = tx
                    .atomic(async |tx| {
                        tx.save_ticket(&ticket("inner", false, &[], &[])).await?;
                        anyhow::bail!("boom")
                    })
                    .await;
                assert!(inner.is_err());
                Ok(())
            })
            .await
        })
        .await?;
    assert_eq!(
        ids(&store.tickets().await?, |t| &t.id),
        vec!["outer", "third", "second", "first"]
    );
    assert_eq!(
        ids(&store.tickets_open().await?, |t| &t.id),
        vec!["outer", "third", "first"]
    );
    assert_eq!(
        ids(&store.tickets_active().await?, |t| &t.id),
        vec!["third", "first"]
    );
    assert_eq!(
        ids(&store.threads().await?, |t| &t.key),
        vec!["github:1", "github:2", "github:3"]
    );
    assert_eq!(
        ids(&store.threads_active().await?, |t| &t.key),
        vec!["github:1", "github:3"]
    );
    assert_eq!(
        ids(&store.decisions_by_thread("github:1").await?, |d| &d.id),
        vec!["d2", "d1"]
    );
    assert_eq!(
        store.ticket("first").await?.map(|t| t.title),
        Some("again".to_owned())
    );
    Ok(())
}

#[tokio::test]
async fn reads_percent_and_underscore_in_a_key_prefix_and_a_search_as_written() -> anyhow::Result<()>
{
    let (_dir, store) = open_store().await?;
    store
        .atomic(async |tx| {
            for name in ["a%b", "axb", "a_c", "abc"] {
                tx.kv_set(name, &json!(1)).await?;
            }
            for (id, text) in [
                ("p", "grew 50% today"),
                ("q", "grew 500 today"),
                ("r", "see a_b"),
                ("s", "see axb"),
            ] {
                tx.save_trigger(&trigger(
                    id,
                    "feishu",
                    &[("feishu", "oc_1:om_1")],
                    "2026-10-01T00:00:00.000Z",
                    text,
                ))
                .await?;
            }
            Ok(())
        })
        .await?;
    assert_eq!(store.kv_keys("a%").await?, vec!["a%b"]);
    assert_eq!(store.kv_keys("a_").await?, vec!["a_c"]);
    assert_eq!(ids(&store.search("50%", 20).await?, |t| &t.id), vec!["p"]);
    assert_eq!(ids(&store.search("a_b", 20).await?, |t| &t.id), vec!["r"]);
    Ok(())
}

#[tokio::test]
async fn keeps_one_receipt_per_message_id() -> anyhow::Result<()> {
    let (_dir, store) = open_store().await?;
    let message = OutboundMessage {
        id: "m1".into(),
        channel: codex_superagent_extension::core::ChannelRef {
            source: "codext".into(),
            thread: "pm/TR-1".into(),
            address: Default::default(),
        },
        text: "done".into(),
        kind: MessageKind::Reply,
        ticket_id: Some("t1".into()),
    };
    let (first, second) = store
        .atomic(async |tx| {
            Ok((
                tx.receipt_add(&message).await?,
                tx.receipt_add(&message).await?,
            ))
        })
        .await?;
    assert_eq!((first, second), (true, false));
    assert_eq!(store.reply_status("m1").await?, Some(ReplyStatus::Pending));
    assert_eq!(
        ids(&store.receipts_waiting().await?, |r| &r.message.id),
        vec!["m1"]
    );
    store
        .atomic(async |tx| {
            tx.receipt_settle(&message, Settle::Sent, "2026-11-02T00:00:00.000Z")
                .await
        })
        .await?;
    assert_eq!(store.reply_status("m1").await?, Some(ReplyStatus::Sent));
    assert!(store.receipts_waiting().await?.is_empty());
    // A withdrawn message that was settled while being sent keeps a receipt, so its id is never queued again.
    let withdrawn = store
        .atomic(async |tx| tx.receipt_withdraw("m1").await)
        .await?;
    assert!(!withdrawn);
    Ok(())
}
