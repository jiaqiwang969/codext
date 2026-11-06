//! The PM's tools: `superagent_route` and the ticket commands (prototype `src/app/tools/main.ts`).

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

use codex_extension_api::FunctionCallError;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolSpec;
use codex_tools::JsonSchema;
use codex_tools::ToolExposure;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use super::IM_WORDS;
use super::NOTE_WORDS;
use super::json_output;
use super::parse_arguments;
use super::word_count;
use crate::cards::ticket_card;
use crate::core::Decision;
use crate::core::DecisionKind;
use crate::core::Reason;
use crate::core::UserError;
use crate::core::followup_owners;
use crate::core::reason_text;
use crate::pm::EngineError;
use crate::pm::PmRuntime;
use crate::pm::RouteOutcome;
use crate::store::Store;

pub const ROUTE_TOOL: &str = "superagent_route";
pub const TICKET_NOTE_TOOL: &str = "superagent_ticket_note";
pub const TICKET_RETRY_TOOL: &str = "superagent_ticket_retry";
pub const REQUEST_STATUS_TOOL: &str = "superagent_request_status";
pub const TRIGGER_REQUEUE_TOOL: &str = "superagent_trigger_requeue";

const TITLE_CHARS: usize = 200;
const WORKTREE_CHARS: usize = 32;
const REASON_CHARS: usize = 500;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RouteKind {
    Accept,
    Attach,
    Ignore,
    Stop,
}

impl RouteKind {
    fn name(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Attach => "attach",
            Self::Ignore => "ignore",
            Self::Stop => "stop",
        }
    }
}

/// One routing decision; each kind's fields are checked as its item runs, so an item that lacks
/// one, or whose value breaks a limit, is refused alone.
#[derive(Clone, Debug, Deserialize)]
pub struct RouteItem {
    pub kind: RouteKind,
    pub trigger_id: String,
    #[serde(default)]
    pub ticket_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub worktree_name: Option<String>,
    #[serde(default)]
    pub acknowledgement: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RouteRequest {
    pub decisions: Vec<RouteItem>,
}

/// The worktree name an accept gives: one to four short lowercase words, kebab-case, 32 characters at most.
pub fn valid_worktree_name(name: &str) -> bool {
    name.chars().count() <= WORKTREE_CHARS
        && !name.is_empty()
        && name.split('-').count() <= 4
        && name.split('-').all(|word| {
            !word.is_empty()
                && word
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
        && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
}

pub const WORKTREE_RULE: &str =
    "Use 1–4 short English words in lowercase kebab-case, at most 32 characters";

fn blank(value: Option<&String>) -> bool {
    value.is_none_or(|v| v.trim().is_empty())
}

/// A field's check: the problem with a value, or none.
type FieldCheck = fn(&str) -> Option<String>;
/// A field, its value, the kinds that read it, the kinds that need it, and its check.
type FieldRule<'a> = (
    &'static str,
    Option<&'a String>,
    &'static [RouteKind],
    &'static [RouteKind],
    FieldCheck,
);

/// Why an item cannot run: a field its kind needs is missing, or one it reads breaks its limit.
pub fn invalid(item: &RouteItem) -> Option<String> {
    let kind = item.kind;
    let reads = |kinds: &[RouteKind]| kinds.contains(&kind);
    let fields: [FieldRule<'_>; 6] = [
        (
            "ticket_id",
            item.ticket_id.as_ref(),
            &[RouteKind::Attach, RouteKind::Stop],
            &[RouteKind::Attach, RouteKind::Stop],
            |_| None,
        ),
        (
            "title",
            item.title.as_ref(),
            &[RouteKind::Accept],
            &[RouteKind::Accept],
            |v| {
                (v.chars().count() > TITLE_CHARS)
                    .then(|| format!("at most {TITLE_CHARS} characters"))
            },
        ),
        (
            "worktree_name",
            item.worktree_name.as_ref(),
            &[RouteKind::Accept],
            &[RouteKind::Accept],
            |v| (!valid_worktree_name(v)).then(|| WORKTREE_RULE.to_owned()),
        ),
        (
            "acknowledgement",
            item.acknowledgement.as_ref(),
            &[RouteKind::Accept],
            &[RouteKind::Accept],
            |v| {
                (word_count(v) > IM_WORDS).then(|| {
                    format!(
                        "An acknowledgement has at most {IM_WORDS} words; write one short sentence"
                    )
                })
            },
        ),
        (
            "note",
            item.note.as_ref(),
            &[RouteKind::Accept, RouteKind::Attach],
            &[],
            |v| {
                (word_count(v) > NOTE_WORDS).then(|| format!("Routing note must not exceed {NOTE_WORDS} words. Shorten it; do not split it across deliveries"))
            },
        ),
        (
            "reason",
            item.reason.as_ref(),
            &[RouteKind::Ignore, RouteKind::Stop],
            &[RouteKind::Ignore],
            |v| {
                (v.chars().count() > REASON_CHARS)
                    .then(|| format!("at most {REASON_CHARS} characters"))
            },
        ),
    ];
    for (name, value, read_by, needed_by, check) in fields {
        if !reads(read_by) {
            continue;
        }
        if blank(value) {
            if needed_by.contains(&kind) {
                return Some(format!("{} needs {name}", kind.name()));
            }
            continue;
        }
        if let Some(problem) = value.and_then(|v| check(v)) {
            return Some(format!("{name}: {problem}"));
        }
    }
    None
}

struct Routed {
    decision: Option<DecisionKind>,
    reason: String,
    ticket_id: Option<String>,
    trigger_ids: Vec<String>,
}

fn from_decision(decision: &Decision) -> Routed {
    Routed {
        decision: Some(decision.kind),
        reason: reason_text(&decision.reason),
        ticket_id: decision.ticket_id.clone(),
        trigger_ids: decision.trigger_ids.clone(),
    }
}

enum Outcome {
    Routed(Routed),
    Refused(String),
}

/// Carries out one routing decision as its engine command. `decided` holds the Triggers this call's
/// earlier items decided, with their batches.
async fn route_item(
    runtime: &PmRuntime,
    item: &RouteItem,
    decided: &HashMap<String, String>,
) -> Result<Outcome, EngineError> {
    // A Trigger an earlier item decided went with that item's Decision, also as a member of its batch.
    if let Some(earlier) = decided.get(&item.trigger_id)
        && item.kind == RouteKind::Ignore
    {
        return Ok(Outcome::Refused(format!(
            "Decided in this call with {earlier}, whose Decision took its batch"
        )));
    }
    let note = item.note.clone().filter(|n| !n.trim().is_empty());
    match item.kind {
        RouteKind::Accept => {
            let before = runtime
                .store
                .disposition(&item.trigger_id)
                .await?
                .and_then(|d| d.decision_id);
            let title = item.title.clone().unwrap_or_default();
            let decision = runtime
                .engine
                .decide(
                    item.trigger_id.clone(),
                    RouteOutcome::Accept {
                        title: title.clone(),
                        scope: title,
                        reason: Reason::code("explicitRouted"),
                        acknowledgement: item.acknowledgement.clone(),
                        name: item.worktree_name.clone(),
                        note,
                    },
                )
                .await?;
            // A Decision that created no ticket is the item's refusal; one that existed before keeps what it was accepted with.
            let Some(ticket_id) = decision
                .ticket_id
                .clone()
                .filter(|_| decision.kind == DecisionKind::Accept)
            else {
                return Ok(Outcome::Refused(format!(
                    "Ticket was not created: {}",
                    reason_text(&decision.reason)
                )));
            };
            if before.as_deref() == Some(decision.id.as_str()) {
                return Ok(Outcome::Routed(Routed {
                    decision: Some(DecisionKind::Accept),
                    reason: format!(
                        "Already accepted as {ticket_id}; title and acknowledgement unchanged"
                    ),
                    ticket_id: Some(ticket_id),
                    trigger_ids: decision.trigger_ids,
                }));
            }
            Ok(Outcome::Routed(from_decision(&decision)))
        }
        RouteKind::Attach => {
            let ticket = runtime
                .engine
                .attach(
                    item.ticket_id.clone().unwrap_or_default(),
                    item.trigger_id.clone(),
                    note,
                )
                .await?;
            Ok(Outcome::Routed(Routed {
                decision: None,
                reason:
                    "Attached: the ticket follows the trigger's thread and receives the trigger"
                        .to_owned(),
                ticket_id: Some(ticket.id),
                trigger_ids: Vec::new(),
            }))
        }
        // Through `decide`, so only a person's message the gate admits with explicit strength stops a ticket.
        RouteKind::Stop => {
            let reason = match item.reason.clone().filter(|r| !r.trim().is_empty()) {
                Some(text) => Reason::said(&text),
                None => Reason::code("humanStop"),
            };
            let decision = runtime
                .engine
                .decide(
                    item.trigger_id.clone(),
                    RouteOutcome::Stop {
                        ticket_id: item.ticket_id.clone().unwrap_or_default(),
                        reason,
                    },
                )
                .await?;
            Ok(Outcome::Routed(from_decision(&decision)))
        }
        RouteKind::Ignore => {
            let decision = runtime
                .engine
                .decide(
                    item.trigger_id.clone(),
                    RouteOutcome::Ignore {
                        reason: Reason::said(item.reason.as_deref().unwrap_or_default()),
                    },
                )
                .await?;
            Ok(Outcome::Routed(from_decision(&decision)))
        }
    }
}

async fn with_ticket(store: &Store, mut result: Value, ticket_id: Option<&str>) -> Value {
    if let Some(id) = ticket_id
        && let Ok(Some(ticket)) = store.ticket(id).await
    {
        result["ticket"] = ticket_card(&ticket);
    }
    result
}

/// `superagent_route`: routing decisions for several Triggers in one call, in order. Each runs as
/// its own engine command; one that is refused, or fails, is answered so, and the others still run.
pub async fn route(runtime: &PmRuntime, request: RouteRequest) -> Result<Value, FunctionCallError> {
    if request.decisions.is_empty() || request.decisions.len() > 20 {
        return Err(FunctionCallError::RespondToModel(
            "decisions holds 1 to 20 items".to_owned(),
        ));
    }
    let mut decided: HashMap<String, String> = HashMap::new();
    let mut results = Vec::with_capacity(request.decisions.len());
    for item in &request.decisions {
        let mut head = json!({ "trigger_id": item.trigger_id, "kind": item.kind.name() });
        if let Some(problem) = invalid(item) {
            head["refused"] = json!(problem);
            results.push(head);
            continue;
        }
        match route_item(runtime, item, &decided).await {
            Ok(Outcome::Refused(why)) => {
                head["refused"] = json!(why);
            }
            Ok(Outcome::Routed(routed)) => {
                for id in &routed.trigger_ids {
                    decided
                        .entry(id.clone())
                        .or_insert_with(|| item.trigger_id.clone());
                }
                if let Some(decision) = routed.decision {
                    head["decision"] = json!(decision);
                }
                head["reason"] = json!(routed.reason);
                head = with_ticket(&runtime.store, head, routed.ticket_id.as_deref()).await;
            }
            Err(EngineError::Refused(err)) => {
                head["refused"] = json!(err.message);
            }
            // An unexpected failure recorded nothing, which the item may be sent again for.
            Err(EngineError::Other(err)) => {
                head["refused"] = json!(format!("Not recorded: {err}; retry this item"));
            }
        }
        results.push(head);
    }
    Ok(json!({ "results": results }))
}

#[derive(Debug, Deserialize)]
pub struct TicketArgs {
    pub ticket_id: String,
}

#[derive(Debug, Deserialize)]
pub struct TicketNoteArgs {
    pub ticket_id: String,
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct TriggerArgs {
    pub trigger_id: String,
}

fn refusal(err: EngineError) -> FunctionCallError {
    FunctionCallError::RespondToModel(err.to_string())
}

pub async fn ticket_retry(
    runtime: &PmRuntime,
    args: TicketArgs,
) -> Result<Value, FunctionCallError> {
    let ticket = runtime
        .engine
        .retry_ticket(args.ticket_id)
        .await
        .map_err(refusal)?;
    Ok(with_ticket(
        &runtime.store,
        json!({ "reason": "Started a new assignment" }),
        Some(&ticket.id),
    )
    .await)
}

pub async fn request_status(
    runtime: &PmRuntime,
    args: TicketArgs,
) -> Result<Value, FunctionCallError> {
    let asked = runtime
        .engine
        .request_status(args.ticket_id.clone())
        .await
        .map_err(refusal)?;
    let reason = if asked {
        "Asked the worker for a status report"
    } else {
        "The worker is not waiting; nothing was sent"
    };
    Ok(with_ticket(
        &runtime.store,
        json!({ "reason": reason }),
        Some(&args.ticket_id),
    )
    .await)
}

pub async fn ticket_note(
    runtime: &PmRuntime,
    args: TicketNoteArgs,
) -> Result<Value, FunctionCallError> {
    let text = args.text.trim().to_owned();
    if text.is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "text is empty".to_owned(),
        ));
    }
    if word_count(&text) > IM_WORDS {
        return Err(FunctionCallError::RespondToModel(format!(
            "At most {IM_WORDS} words"
        )));
    }
    runtime
        .engine
        .note(args.ticket_id.clone(), text)
        .await
        .map_err(refusal)?;
    Ok(with_ticket(
        &runtime.store,
        json!({ "reason": "Noted for the worker" }),
        Some(&args.ticket_id),
    )
    .await)
}

pub async fn trigger_requeue(
    runtime: &PmRuntime,
    args: TriggerArgs,
) -> Result<Value, FunctionCallError> {
    // A follow-up that reached a ticket is its worker's to judge; the PM may not take it back.
    if let Some(trigger) = runtime
        .store
        .trigger(&args.trigger_id)
        .await
        .map_err(|e| FunctionCallError::Fatal(e.to_string()))?
    {
        let records = StoreRecords::load(&runtime.store, &trigger)
            .await
            .map_err(|e| FunctionCallError::Fatal(e.to_string()))?;
        let owners = followup_owners(&trigger, &records);
        if !owners.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                UserError::new("workerDecides", &[("tickets", &owners.join(", "))]).message,
            ));
        }
    }
    let queued = runtime
        .engine
        .reevaluate(args.trigger_id)
        .await
        .map_err(refusal)?;
    Ok(json!({ "queued": queued }))
}

/// The stored records `followup_owners` reads, loaded once for a Trigger.
struct StoreRecords {
    tickets: HashMap<String, crate::core::Ticket>,
    threads: HashMap<String, crate::core::Thread>,
    disposition: Option<crate::core::TriggerDisposition>,
}

impl StoreRecords {
    async fn load(store: &Store, trigger: &crate::core::Trigger) -> anyhow::Result<Self> {
        let disposition = store.disposition(&trigger.id).await?;
        let mut tickets = HashMap::new();
        if let Some(d) = &disposition {
            for id in &d.fanout {
                if let Some(ticket) = store.ticket(id).await? {
                    tickets.insert(id.clone(), ticket);
                }
            }
        }
        let mut threads = HashMap::new();
        for reference in &trigger.threads {
            let key = crate::core::thread_key(reference);
            if let Some(thread) = store.thread(&key).await? {
                threads.insert(key, thread);
            }
        }
        Ok(Self {
            tickets,
            threads,
            disposition,
        })
    }
}

impl crate::core::FollowupRecords for StoreRecords {
    fn ticket(&self, id: &str) -> Option<crate::core::Ticket> {
        self.tickets.get(id).cloned()
    }

    fn thread(&self, key: &str) -> Option<crate::core::Thread> {
        self.threads.get(key).cloned()
    }

    fn disposition(&self, _trigger_id: &str) -> Option<crate::core::TriggerDisposition> {
        self.disposition.clone()
    }
}

// ---- the codext tools ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PmToolKind {
    Route,
    TicketNote,
    TicketRetry,
    RequestStatus,
    TriggerRequeue,
}

impl PmToolKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Route => ROUTE_TOOL,
            Self::TicketNote => TICKET_NOTE_TOOL,
            Self::TicketRetry => TICKET_RETRY_TOOL,
            Self::RequestStatus => REQUEST_STATUS_TOOL,
            Self::TriggerRequeue => TRIGGER_REQUEUE_TOOL,
        }
    }
}

fn ticket_id_schema() -> BTreeMap<String, JsonSchema> {
    BTreeMap::from([(
        "ticket_id".to_owned(),
        JsonSchema::string(Some("The ticket.".to_owned())),
    )])
}

/// The schema of one `superagent_route` item: its shape only; each field's limit is stated in its
/// description and checked as the item runs.
pub fn route_item_schema() -> JsonSchema {
    let properties = BTreeMap::from([
        (
            "kind".to_owned(),
            JsonSchema::string_enum(
                vec![json!("accept"), json!("attach"), json!("ignore"), json!("stop")],
                Some("accept an explicit request as a new ticket, attach a trigger to an existing ticket, stop a ticket on a person's stop message, or ignore a candidate that needs no ticket or stop.".to_owned()),
            ),
        ),
        ("trigger_id".to_owned(), JsonSchema::string(Some("The candidate card's trigger_id; deciding it decides its batch.".to_owned()))),
        ("ticket_id".to_owned(), JsonSchema::string(Some("attach and stop: the ticket.".to_owned()))),
        (
            "title".to_owned(),
            JsonSchema::string(Some(format!("accept: a short label of at most {TITLE_CHARS} characters describing the original request; no added requirements or implementation advice."))),
        ),
        ("worktree_name".to_owned(), JsonSchema::string(Some(format!("accept: {WORKTREE_RULE}; the ticket is named tNNN_<name> after it.")))),
        (
            "acknowledgement".to_owned(),
            JsonSchema::string(Some(format!("accept: one brief sentence of at most {IM_WORDS} words naming the concrete change and intended result in the source language. It is sent as the acknowledgement on acceptance, so do not quote the source. No generic I will handle it."))),
        ),
        (
            "note".to_owned(),
            JsonSchema::string(Some(format!("accept and attach: optional routing background or explicit human constraint, at most {NOTE_WORDS} words. Never add implementation instructions or replace the original source."))),
        ),
        ("reason".to_owned(), JsonSchema::string(Some(format!("ignore and stop: why, at most {REASON_CHARS} characters; an ignore needs it.")))),
    ]);
    JsonSchema::object(
        properties,
        Some(vec!["kind".to_owned(), "trigger_id".to_owned()]),
        Some(false.into()),
    )
}

pub fn tool_spec(kind: PmToolKind) -> ToolSpec {
    let (description, parameters) = match kind {
        PmToolKind::Route => (
            "Route candidates, one item each by the card's trigger_id, in one call: accept an explicit request as a new ticket, attach a trigger to an existing ticket, stop a ticket on a person's stop message, or ignore a candidate that needs no ticket or stop. Items run in order and each is answered on its own; a refused item leaves the others.".to_owned(),
            JsonSchema::object(
                BTreeMap::from([(
                    "decisions".to_owned(),
                    JsonSchema::array(route_item_schema(), Some("1 to 20 decisions, in order.".to_owned())),
                )]),
                Some(vec!["decisions".to_owned()]),
                Some(false.into()),
            ),
        ),
        PmToolKind::TicketRetry => (
            "Start a new assignment on an explicit human request to retry a ticket whose worker reported failed or blocked, or whose input was not delivered; the stalled one ends.".to_owned(),
            JsonSchema::object(ticket_id_schema(), Some(vec!["ticket_id".to_owned()]), Some(false.into())),
        ),
        PmToolKind::RequestStatus => (
            "Ask the worker of an idle ticket, one whose turn ended without a report, for a status report; for other tickets it sends nothing and returns the card.".to_owned(),
            JsonSchema::object(ticket_id_schema(), Some(vec!["ticket_id".to_owned()]), Some(false.into())),
        ),
        PmToolKind::TicketNote => (
            "Tell a ticket's worker a fact from other work, such as what another ticket found; it grants no authority.".to_owned(),
            JsonSchema::object(
                BTreeMap::from([
                    ("ticket_id".to_owned(), JsonSchema::string(Some("The ticket.".to_owned()))),
                    ("text".to_owned(), JsonSchema::string(Some(format!("The fact, at most {IM_WORDS} words.")))),
                ]),
                Some(vec!["ticket_id".to_owned(), "text".to_owned()]),
                Some(false.into()),
            ),
        ),
        PmToolKind::TriggerRequeue => (
            "Queue a trigger for a fresh assessment; a follow-up that reached a ticket comes back only from its worker.".to_owned(),
            JsonSchema::object(
                BTreeMap::from([("trigger_id".to_owned(), JsonSchema::string(Some("The trigger.".to_owned())))]),
                Some(vec!["trigger_id".to_owned()]),
                Some(false.into()),
            ),
        ),
    };
    ToolSpec::Function(ResponsesApiTool {
        name: kind.name().to_owned(),
        description,
        strict: false,
        defer_loading: None,
        parameters,
        output_schema: None,
    })
}

/// One of the PM's tools, bound to the runtime.
pub struct PmTool {
    kind: PmToolKind,
    runtime: Arc<PmRuntime>,
}

impl PmTool {
    pub fn new(kind: PmToolKind, runtime: Arc<PmRuntime>) -> Self {
        Self { kind, runtime }
    }

    /// Runs the tool on its JSON arguments; the executor's `handle` and the tests share this.
    pub async fn run(&self, arguments: &str) -> Result<Value, FunctionCallError> {
        match self.kind {
            PmToolKind::Route => route(&self.runtime, parse_arguments(arguments)?).await,
            PmToolKind::TicketNote => ticket_note(&self.runtime, parse_arguments(arguments)?).await,
            PmToolKind::TicketRetry => {
                ticket_retry(&self.runtime, parse_arguments(arguments)?).await
            }
            PmToolKind::RequestStatus => {
                request_status(&self.runtime, parse_arguments(arguments)?).await
            }
            PmToolKind::TriggerRequeue => {
                trigger_requeue(&self.runtime, parse_arguments(arguments)?).await
            }
        }
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for PmTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(self.kind.name())
    }

    fn spec(&self) -> ToolSpec {
        tool_spec(self.kind)
    }

    /// Astra works in code mode; these tools stay direct calls, not nested code-mode calls (design 1.1).
    fn exposure(&self) -> ToolExposure {
        ToolExposure::DirectModelOnly
    }

    fn handle<'a>(&'a self, invocation: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(async move {
            let arguments = invocation.function_arguments()?.to_owned();
            let value = self.run(&arguments).await?;
            Ok(json_output(value))
        })
    }
}
