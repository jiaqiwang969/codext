//! The intake glue: each turn the user submits in a PM's or a worker's conversation becomes a
//! Trigger (design §4.1), through codext's turn-input contributor. Replaces the A0
//! UserPromptSubmit hook and `triggers.jsonl`.

use std::sync::Arc;
use std::sync::Weak;

use codex_extension_api::ContextualUserFragment;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionMetrics;
use codex_extension_api::TurnInputContext;
use codex_extension_api::TurnInputContributor;
use codex_protocol::user_input::UserInput;
use serde_json::Map;
use serde_json::Value;

use crate::SuperagentService;
use crate::core::ActorRef;
use crate::core::Addressed;
use crate::core::ChannelRef;
use crate::core::Engine;
use crate::core::Trigger;
use crate::core::now_iso;
use crate::core::thread_rules::SOURCE;
use crate::core::thread_rules::Spoken;
use crate::core::thread_rules::place;
use crate::ids::key;
use crate::router::RouterMode;

/// The user's id as the one allowed sender.
pub const USER: &str = "user";

/// How every text the engine hands to a thread starts: the candidate card, the outbox labels,
/// the inputs a worker receives. Items the persistent queue still holds across a restart are not
/// in the in-memory ledger, so the intake also recognises them by shape.
const ENGINE_LABELS: &[&str] = &[
    "[候选卡",
    "[确认]",
    "[通知]",
    "[回答]",
    "[待决问题]",
    "[结果]",
    "[输入 ",
    "[上下文 ",
    "[已恢复 ",
    "[规则变化 ",
    "[重开 ",
    "[备注 ",
];

pub fn engine_delivery(text: &str) -> bool {
    let text = text.trim_start();
    ENGINE_LABELS.iter().any(|label| text.starts_with(label)) || assignment_card(text)
}

/// A worker's first assignment (`executor::codext_thread` compose): `工单 <name>：<title>`, then
/// its `授权范围：` line. A person rarely writes that shape, and a queued one survives a restart.
fn assignment_card(text: &str) -> bool {
    let mut lines = text.lines();
    let Some((name, _title)) = lines
        .next()
        .and_then(|line| line.strip_prefix("工单 "))
        .and_then(|head| head.split_once('：'))
    else {
        return false;
    };
    !name.is_empty()
        && !name.contains(char::is_whitespace)
        && lines
            .next()
            .is_some_and(|line| line.starts_with("授权范围："))
}

pub struct SuperagentIntake {
    service: Weak<SuperagentService>,
}

impl SuperagentIntake {
    pub fn new(service: &Arc<SuperagentService>) -> Self {
        Self {
            service: Arc::downgrade(service),
        }
    }
}

/// The text of a turn's user input, as one message.
pub fn text_of(user_input: &[UserInput]) -> String {
    let parts: Vec<&str> = user_input
        .iter()
        .filter_map(|input| match input {
            UserInput::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    parts.join("\n").trim().to_owned()
}

/// Builds the Trigger of a turn, placed by the Thread rules.
pub async fn trigger_of(
    engine: &Engine,
    spoken: &Spoken,
    thread_id: &str,
    turn_id: &str,
    text: &str,
) -> anyhow::Result<Trigger> {
    trigger_of_with(engine, spoken, thread_id, turn_id, text, false).await
}

/// `steered` marks a message that joined a running turn: it gets its own id beside the turn's
/// first message (the id of a turn's first message is the turn's).
pub async fn trigger_of_with(
    engine: &Engine,
    spoken: &Spoken,
    thread_id: &str,
    turn_id: &str,
    text: &str,
    steered: bool,
) -> anyhow::Result<Trigger> {
    let id = if steered {
        key([SOURCE, thread_id, turn_id, "steered", text])
    } else {
        key([SOURCE, thread_id, turn_id])
    };
    let mut names = Vec::new();
    for name in crate::core::thread_rules::ticket_names(text) {
        if let Some(ticket) = engine.ticket_of_name(&name).await? {
            names.push((name, ticket));
        }
    }
    let placed = place(spoken, text, &id, |name| {
        names
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, t)| t.clone())
    });
    let mut meta = Map::new();
    meta.insert("turnId".to_owned(), Value::String(turn_id.to_owned()));
    meta.insert("thread".to_owned(), Value::String(thread_id.to_owned()));
    if !placed.mentions.is_empty() {
        meta.insert(
            "mentions".to_owned(),
            Value::Array(
                placed
                    .mentions
                    .iter()
                    .map(|m| Value::String(m.clone()))
                    .collect(),
            ),
        );
    }
    if let Spoken::Worker { ticket_id } = spoken {
        meta.insert("worker".to_owned(), Value::String(ticket_id.clone()));
    }
    if steered {
        meta.insert("steered".to_owned(), Value::Bool(true));
    }
    Ok(Trigger {
        id,
        source: SOURCE.to_owned(),
        threads: vec![placed.thread],
        actor: ActorRef {
            source: SOURCE.to_owned(),
            id: USER.to_owned(),
        },
        human: true,
        addressed: Some(Addressed::Direct),
        at: now_iso(),
        text: text.to_owned(),
        channel: Some(ChannelRef {
            source: SOURCE.to_owned(),
            thread: thread_id.to_owned(),
            address: [("thread".to_owned(), thread_id.to_owned())]
                .into_iter()
                .collect(),
        }),
        url: None,
        meta,
    })
}

impl TurnInputContributor for SuperagentIntake {
    fn contribute<'a>(
        &'a self,
        input: TurnInputContext<'a>,
        _extension_metrics: Option<Arc<dyn ExtensionMetrics>>,
        _session_store: &'a ExtensionData,
        thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
    ) -> ExtensionFuture<'a, Vec<Box<dyn ContextualUserFragment + Send>>> {
        Box::pin(async move {
            let Some(service) = self.service.upgrade() else {
                return Vec::new();
            };
            let thread_id = thread_store.level_id().to_owned();
            let text = text_of(&input.user_input);
            record_turn(&service, &thread_id, &input.turn_id, &text, false).await;
            Vec::new()
        })
    }
}


/// Records one message a person put into a PM's or a worker's conversation as a Trigger: the turn's
/// first message through the turn-input contributor, a message steered into a running turn through
/// the host (`SuperagentService::steered_input`). Engine deliveries are skipped either way.
pub async fn record_turn(
    service: &SuperagentService,
    thread_id: &str,
    turn_id: &str,
    text: &str,
    steered: bool,
) {
    let Some(runtime) = service.runtime() else {
        return;
    };
    let spoken = if service.pm_threads().is_pm(thread_id) {
        // The observing Router routes nothing, so its messages open no candidates.
        if service.router_mode() == RouterMode::Observe {
            return;
        }
        Spoken::Pm {
            pm_thread: thread_id.to_owned(),
        }
    } else {
        match runtime.engine.worker_ticket(thread_id).await {
            Ok(Some(ticket_id)) => Spoken::Worker { ticket_id },
            Ok(None) => return,
            Err(err) => {
                tracing::warn!("superagent intake could not read the worker index: {err:#}");
                return;
            }
        }
    };
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    // The engine's own deliveries (candidate cards, relays, assignments) arrive as user input
    // too; reading them back as triggers would feed the engine its own output.
    if service.ledger().delivered(text) || engine_delivery(text) {
        tracing::debug!("superagent intake: skipping an engine delivery on {thread_id}");
        return;
    }
    match trigger_of_with(&runtime.engine, &spoken, thread_id, turn_id, text, steered).await {
        Ok(trigger) => {
            if let Err(err) = runtime.engine.receive(trigger).await {
                tracing::warn!("superagent intake could not record the turn: {err:#}");
            }
        }
        Err(err) => tracing::warn!("superagent intake could not place the turn: {err:#}"),
    }
}
