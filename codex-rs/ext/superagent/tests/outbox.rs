//! The outbox: each message once, in order per ticket and conversation, dropped when the
//! conversation is gone (the prototype's `tests/outbox.spec.ts` and the channel rules of
//! `docs/003-execution.md`).

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use codex_protocol::ThreadId;
use codex_superagent_extension::BoxFut;
use codex_superagent_extension::Effects;
use codex_superagent_extension::HealthBoard;
use codex_superagent_extension::Store;
use codex_superagent_extension::core::Assignment;
use codex_superagent_extension::core::AssignmentState;
use codex_superagent_extension::core::Attention;
use codex_superagent_extension::core::ChannelRef;
use codex_superagent_extension::core::Report;
use codex_superagent_extension::core::ReportState;
use codex_superagent_extension::executor::codext_thread::SessionRecord;
use codex_superagent_extension::executor::codext_thread::session_key;
use codex_superagent_extension::host::DeliveryMode;
use codex_superagent_extension::host::GoalStatus;
use codex_superagent_extension::host::SuperagentHost;
use codex_superagent_extension::host::ThreadSpec;
use codex_superagent_extension::ids::key;
use codex_superagent_extension::outbox::install_outbox;
use codex_superagent_extension::pm::PmThreads;
use codex_superagent_extension::store::DropReason;
use codex_superagent_extension::store::MessageKind;
use codex_superagent_extension::store::OutboundMessage;
use codex_superagent_extension::store::ReplyStatus;
use common::FakeClock;
use common::T0;
use common::open_store;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

/// Records deliveries; refuses those whose text contains `fail_containing`.
#[derive(Default)]
struct FakeHost {
    delivered: Mutex<Vec<(String, String, DeliveryMode)>>,
    fail_containing: Mutex<Option<String>>,
    resume_fails: Mutex<Vec<String>>,
}

impl FakeHost {
    fn delivered(&self) -> Vec<(String, String, DeliveryMode)> {
        self.delivered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn texts(&self) -> Vec<String> {
        self.delivered()
            .into_iter()
            .map(|(_, text, _)| text)
            .collect()
    }

    fn fail_messages_containing(&self, text: Option<&str>) {
        *self
            .fail_containing
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = text.map(str::to_owned);
    }

    fn fail_resume_of(&self, thread: &ThreadId) {
        self.resume_fails
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(thread.to_string());
    }
}

impl SuperagentHost for FakeHost {
    fn start_thread(&self, _spec: ThreadSpec) -> BoxFut<'_, anyhow::Result<ThreadId>> {
        Box::pin(async { anyhow::bail!("not in this test") })
    }

    fn bind_profile(&self, _thread: ThreadId, _profile: String) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn resume_thread(
        &self,
        thread: ThreadId,
        _role: Option<String>,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        let fails = self
            .resume_fails
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&thread.to_string());
        Box::pin(async move {
            if fails {
                anyhow::bail!("no rollout for {thread}")
            }
            Ok(())
        })
    }

    fn deliver(
        &self,
        thread: ThreadId,
        text: String,
        mode: DeliveryMode,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        let fails = self
            .fail_containing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|needle| text.contains(needle.as_str()))
            || self
                .resume_fails
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains(&thread.to_string());
        if !fails {
            self.delivered
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((thread.to_string(), text, mode));
        }
        Box::pin(async move {
            if fails {
                anyhow::bail!("thread {thread} refused the message")
            }
            Ok(())
        })
    }

    fn interrupt(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn clean_background(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn background_commands(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<String>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn children(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<ThreadId>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn goal_set(
        &self,
        _thread: ThreadId,
        _objective: String,
        _token_budget: Option<i64>,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn goal_clear(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn goal_status(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Option<GoalStatus>>> {
        Box::pin(async { Ok(None) })
    }
}

struct Setup {
    _dir: TempDir,
    store: Store,
    clock: FakeClock,
    effects: Effects,
    host: Arc<FakeHost>,
    pm_thread: ThreadId,
}

async fn setup() -> anyhow::Result<Setup> {
    let (dir, store) = open_store().await?;
    let clock = FakeClock::at(T0);
    let effects = Effects::new(store.clone(), HealthBoard::new(), clock.clock()).await?;
    effects.start();
    let host = Arc::new(FakeHost::default());
    let pm_thread = ThreadId::new();
    let pm_threads = PmThreads::new(dir.path().join("pm-threads"));
    std::fs::write(pm_threads.path(), format!("{pm_thread}\n"))?;
    install_outbox(
        effects.clone(),
        store.clone(),
        host.clone() as Arc<dyn SuperagentHost>,
        pm_threads,
    )
    .await?;
    Ok(Setup {
        _dir: dir,
        store,
        clock,
        effects,
        host,
        pm_thread,
    })
}

fn message(
    id: &str,
    thread: &ThreadId,
    text: &str,
    kind: MessageKind,
    ticket: Option<&str>,
) -> OutboundMessage {
    OutboundMessage {
        id: id.to_owned(),
        channel: ChannelRef {
            source: "codext".into(),
            thread: thread.to_string(),
            address: BTreeMap::from([("thread".to_owned(), thread.to_string())]),
        },
        text: text.to_owned(),
        kind,
        ticket_id: ticket.map(str::to_owned),
    }
}

/// Queues a message as the engine's replies do: a receipt and a `reply` effect in one transaction.
async fn queue(s: &Setup, message: &OutboundMessage) -> anyhow::Result<bool> {
    s.store
        .atomic(async |tx| {
            let new = tx.receipt_add(message).await?;
            if new {
                s.effects
                    .enqueue(tx, "reply", &message.id, &json!({}))
                    .await?;
            }
            Ok(new)
        })
        .await
}

/// A worker's thread, as the executor records it.
async fn worker_session(s: &Setup, ticket_id: &str, thread: &ThreadId) -> anyhow::Result<()> {
    let record = SessionRecord {
        account: "A".into(),
        thread_id: thread.to_string(),
        cwd: String::new(),
        model: None,
        turns: 1,
        tokens_at_start: 0,
    };
    s.store
        .atomic(async |tx| tx.kv_set(&session_key(ticket_id), &record).await)
        .await
}

#[tokio::test]
async fn sends_a_queued_message_once_with_its_kind_in_front() -> anyhow::Result<()> {
    let s = setup().await?;
    let ack = message(
        "ack-1",
        &s.pm_thread,
        "收到，先确认范围。",
        MessageKind::Acknowledgement,
        Some("t017"),
    );
    assert!(queue(&s, &ack).await?);
    s.effects.idle(None).await;
    assert_eq!(
        s.host.delivered(),
        vec![(
            s.pm_thread.to_string(),
            "[确认] 收到，先确认范围。".to_owned(),
            DeliveryMode::Queue
        )]
    );
    assert_eq!(
        s.store.reply_status("ack-1").await?,
        Some(ReplyStatus::Sent)
    );
    // The same id queued again neither records a new receipt nor goes out again, even when the
    // effect itself is queued once more (as after a restart that replays a command).
    assert!(!queue(&s, &ack).await?);
    s.store
        .atomic(async |tx| s.effects.enqueue(tx, "reply", "ack-1", &json!({})).await)
        .await?;
    s.effects.idle(None).await;
    assert_eq!(s.host.delivered().len(), 1);
    assert!(!s.effects.pending("reply", None).await?);
    Ok(())
}

#[tokio::test]
async fn keeps_a_tickets_messages_to_one_conversation_in_order_and_lets_others_through()
-> anyhow::Result<()> {
    let s = setup().await?;
    s.host.fail_messages_containing(Some("第一条"));
    let pm = &s.pm_thread;
    queue(
        &s,
        &message("a", pm, "第一条", MessageKind::Reply, Some("t1")),
    )
    .await?;
    queue(
        &s,
        &message("b", pm, "第二条", MessageKind::Reply, Some("t1")),
    )
    .await?;
    queue(
        &s,
        &message("c", pm, "别的工单", MessageKind::Reply, Some("t2")),
    )
    .await?;
    s.effects.idle(None).await;
    // The first fails; the second waits behind it; another ticket's message goes out.
    assert_eq!(s.host.texts(), vec!["[回答] 别的工单"]);
    assert!(s.effects.pending("reply", Some("a")).await?);
    assert!(s.effects.pending("reply", Some("b")).await?);
    // A second failure 5 s later: still held.
    s.clock.advance(5_000);
    s.effects.idle(None).await;
    assert_eq!(s.host.texts(), vec!["[回答] 别的工单"]);
    assert!(s.effects.pending("reply", Some("b")).await?);
    // The third failure, 10 s later again: the earlier message no longer holds the later one.
    s.clock.advance(10_000);
    s.effects.idle(None).await;
    assert_eq!(s.host.texts(), vec!["[回答] 别的工单", "[回答] 第二条"]);
    assert_eq!(s.store.reply_status("b").await?, Some(ReplyStatus::Sent));
    assert_eq!(s.store.reply_status("a").await?, Some(ReplyStatus::Pending));
    // Once the conversation takes it, the first goes out too, once.
    s.host.fail_messages_containing(None);
    s.clock.advance(300_000);
    s.effects.idle(None).await;
    assert_eq!(s.store.reply_status("a").await?, Some(ReplyStatus::Sent));
    assert_eq!(s.host.texts().len(), 3);
    assert!(!s.effects.pending("reply", None).await?);
    Ok(())
}

#[tokio::test]
async fn drops_a_message_whose_conversation_is_gone_and_keeps_its_receipt() -> anyhow::Result<()> {
    let s = setup().await?;
    let unknown = ThreadId::new();
    let lost = message("lost", &unknown, "没人收", MessageKind::Notice, Some("t1"));
    queue(&s, &lost).await?;
    let broken = OutboundMessage {
        channel: ChannelRef {
            source: "codext".into(),
            thread: "not-a-thread-id".into(),
            address: BTreeMap::new(),
        },
        ..message("broken", &unknown, "坏地址", MessageKind::Notice, None)
    };
    queue(&s, &broken).await?;
    // A worker's thread we know of, but whose rollout the host can neither load nor write to.
    let unloadable = ThreadId::new();
    worker_session(&s, "t3", &unloadable).await?;
    s.host.fail_resume_of(&unloadable);
    queue(
        &s,
        &message(
            "unloadable",
            &unloadable,
            "线程没了",
            MessageKind::Reply,
            Some("t3"),
        ),
    )
    .await?;
    s.effects.idle(None).await;
    assert!(s.host.delivered().is_empty());
    for id in ["lost", "broken", "unloadable"] {
        let receipt = s
            .store
            .receipt(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!(id))?;
        assert_eq!(receipt.status, ReplyStatus::Dropped, "{id}");
        assert_eq!(
            receipt.dropped.map(|d| d.reason),
            Some(DropReason::ChannelGone),
            "{id}"
        );
    }
    // A dropped id is never queued again.
    assert!(!queue(&s, &lost).await?);
    assert!(!s.effects.pending("reply", None).await?);
    assert_eq!(s.store.dropped_since("2026-01-01T00:00:00.000Z").await?, 3);
    Ok(())
}

#[tokio::test]
async fn reaches_a_workers_thread_and_labels_replies_by_what_they_carry() -> anyhow::Result<()> {
    let s = setup().await?;
    let worker = ThreadId::new();
    let mut ticket = common::ticket("t017", false, &[AssignmentState::Running], &[]);
    ticket.attention = Some(Attention {
        id: "q1".into(),
        question: "用哪个分支？".into(),
        trigger_id: None,
    });
    s.store
        .atomic(async |tx| tx.save_ticket(&ticket).await)
        .await?;
    worker_session(&s, "t017", &worker).await?;
    let attention_id = key(["attention-reply", "q1"]);
    queue(
        &s,
        &message(
            &attention_id,
            &worker,
            "用哪个分支？",
            MessageKind::Reply,
            Some("t017"),
        ),
    )
    .await?;
    queue(
        &s,
        &message(
            "answer",
            &worker,
            "用 main。",
            MessageKind::Reply,
            Some("t017"),
        ),
    )
    .await?;
    s.effects.idle(None).await;
    let mut ready = ticket.clone();
    ready.attention = None;
    ready.assignments = vec![Assignment {
        report: Some(Report {
            state: ReportState::Ready,
            summary: "修好了".into(),
            at: "2".into(),
        }),
        ..Assignment::new("t017-0", "1")
    }];
    s.store
        .atomic(async |tx| tx.save_ticket(&ready).await)
        .await?;
    queue(
        &s,
        &message(
            "result",
            &worker,
            "修好了，见证据。",
            MessageKind::Reply,
            Some("t017"),
        ),
    )
    .await?;
    // A thread the engine has heard from (a person spoke there) is a conversation too.
    let spoken = ThreadId::new();
    s.store
        .atomic(async |tx| {
            tx.save_thread(&common::thread(&spoken.to_string(), "codext"))
                .await
        })
        .await?;
    queue(
        &s,
        &message("note", &spoken, "工单结束了。", MessageKind::Notice, None),
    )
    .await?;
    s.effects.idle(None).await;
    assert_eq!(
        s.host.texts(),
        vec![
            "[待决问题] 用哪个分支？",
            "[回答] 用 main。",
            "[结果] 修好了，见证据。",
            "[通知] 工单结束了。",
        ]
    );
    let threads: Vec<String> = s
        .host
        .delivered()
        .into_iter()
        .map(|(thread, _, _)| thread)
        .collect();
    assert_eq!(
        threads,
        vec![
            worker.to_string(),
            worker.to_string(),
            worker.to_string(),
            spoken.to_string()
        ]
    );
    Ok(())
}

#[tokio::test]
async fn tries_a_refused_delivery_again_later_without_a_second_receipt() -> anyhow::Result<()> {
    let s = setup().await?;
    s.host.fail_messages_containing(Some("稍后"));
    queue(
        &s,
        &message("later", &s.pm_thread, "稍后再送", MessageKind::Notice, None),
    )
    .await?;
    s.effects.idle(None).await;
    assert!(s.host.delivered().is_empty());
    assert_eq!(
        s.store.reply_status("later").await?,
        Some(ReplyStatus::Pending)
    );
    s.host.fail_messages_containing(None);
    s.clock.advance(4_999);
    s.effects.idle(None).await;
    assert!(s.host.delivered().is_empty(), "the retry waits 5 s");
    s.clock.advance(1);
    s.effects.idle(None).await;
    assert_eq!(s.host.texts(), vec!["[通知] 稍后再送"]);
    assert_eq!(
        s.store.reply_status("later").await?,
        Some(ReplyStatus::Sent)
    );
    assert!(s.store.receipts_waiting().await?.is_empty());
    Ok(())
}
