//! The PM side of the engine (design §5): the engine role the PM's tools call, which PM threads
//! are, the effect handlers that deliver candidate cards, relays and notices into the PM thread,
//! and the extension contribution that gives PM threads their tools.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use codex_extension_api::ExtensionData;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use crate::BoxFut;
use crate::SuperagentService;
use crate::cards::CandidateContext;
use crate::cards::candidate_card;
use crate::core::AcceptMode;
use crate::core::Decision;
use crate::core::Reason;
use crate::core::Strength;
use crate::core::Thread;
use crate::core::Ticket;
use crate::core::Trigger;
use crate::core::TriggerDisposition;
use crate::core::UserError;
use crate::effects::EffectError;
use crate::effects::EffectRun;
use crate::effects::Effects;
use crate::effects::HandlerOptions;
use crate::host::DeliveryMode;
use crate::host::SuperagentHost;
use crate::router::RouterMode;
use crate::store::Store;
use crate::tools::drive::DriveTool;
use crate::tools::pm::PmTool;
use crate::tools::pm::PmToolKind;
use crate::tools::read::ReadTool;

/// Why an engine command did not happen: a refusal the PM can act on, or a failure to retry.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("{0}")]
    Refused(#[from] UserError),
    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for EngineError {
    fn from(err: anyhow::Error) -> Self {
        Self::Other(format!("{err:#}"))
    }
}

/// What the PM receives to decide: bounded facts, never authority (prototype `ports.ts` `Candidate`).
#[derive(Clone, Debug)]
pub struct Candidate {
    pub eval_id: String,
    pub thread: Thread,
    /// The batch, the primary Trigger last.
    pub triggers: Vec<Trigger>,
    pub strength: Strength,
    pub policy_version: String,
    /// Every ticket on the Thread, ended ones included, for duplicate judgement.
    pub tickets: Vec<Ticket>,
    pub mode: AcceptMode,
}

/// A PM decision on a candidate: the explicit kinds of the prototype's `AssessmentOutcome`.
#[derive(Clone, Debug, PartialEq)]
pub enum RouteOutcome {
    Accept {
        title: String,
        scope: String,
        reason: Reason,
        acknowledgement: Option<String>,
        /// The worktree short name the ticket is named after.
        name: Option<String>,
        note: Option<String>,
    },
    Stop {
        ticket_id: String,
        reason: Reason,
    },
    Ignore {
        reason: Reason,
    },
}

/// The engine as the PM's tools see it: the prototype's `EngineForMainSession` and the people's
/// commands the PM relays (`EngineForPeople`). T-12 implements it on the real engine.
pub trait EngineForPm: Send + Sync {
    /// The Eval that holds a Trigger, as its assessor sees it; `None` once it ended.
    fn candidate(&self, trigger_id: String) -> BoxFut<'_, Result<Option<Candidate>, EngineError>>;
    /// The Eval's candidate reached the PM: its deadline counts from now.
    fn candidate_delivered(&self, eval_id: String) -> BoxFut<'_, Result<(), EngineError>>;
    /// Records a Decision for the Eval that holds the Trigger; an accept creates the Ticket.
    fn decide(
        &self,
        trigger_id: String,
        outcome: RouteOutcome,
    ) -> BoxFut<'_, Result<Decision, EngineError>>;
    /// Routes a Trigger to a Ticket it points at; the Ticket then follows the Trigger's Thread.
    fn attach(
        &self,
        ticket_id: String,
        trigger_id: String,
        note: Option<String>,
    ) -> BoxFut<'_, Result<Ticket, EngineError>>;
    /// Asks a waiting worker for a status report; answers whether it asked.
    fn request_status(&self, ticket_id: String) -> BoxFut<'_, Result<bool, EngineError>>;
    /// Gives a Ticket's worker a fact from other work, which grants nothing.
    fn note(&self, ticket_id: String, text: String) -> BoxFut<'_, Result<(), EngineError>>;
    /// Ends a stalled Assignment and starts a new one.
    fn retry_ticket(&self, ticket_id: String) -> BoxFut<'_, Result<Ticket, EngineError>>;
    /// Queues a stored Trigger for a fresh Eval; answers whether it queued it.
    fn reevaluate(&self, trigger_id: String) -> BoxFut<'_, Result<bool, EngineError>>;
}

/// Which threads are PMs: one thread id per line in `<sqlite_home>/superagent/pm-threads`, the
/// first being the PM that candidates go to. The file is read on each use, so the org can edit it.
#[derive(Clone, Debug)]
pub struct PmThreads {
    path: PathBuf,
}

impl PmThreads {
    pub const FILE: &'static str = "superagent/pm-threads";

    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn under(home: &Path) -> Self {
        Self::new(home.join(Self::FILE))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn list(&self) -> Vec<String> {
        std::fs::read_to_string(&self.path)
            .map(|text| {
                text.lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty() && !line.starts_with('#'))
                    // a line may carry the PM's project after the id (pm_threads.rs)
                    .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn is_pm(&self, thread_id: &str) -> bool {
        self.list().iter().any(|id| id == thread_id)
    }

    /// The PM candidates and relays go to.
    pub fn primary(&self) -> Option<String> {
        self.list().into_iter().next()
    }
}

/// What the PM's tools and the delivery handlers work with.
pub struct PmRuntime {
    pub engine: Arc<dyn EngineForPm>,
    pub host: Arc<dyn SuperagentHost>,
    pub store: Store,
    pub effects: Effects,
    pub pm_threads: PmThreads,
}

/// The payload of an `assessment` effect; its key is the Eval id.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssessmentPayload {
    trigger_id: String,
}

/// The payload of a `relay` effect; its key is the request id.
#[derive(Debug, Deserialize)]
struct RelayPayload {
    text: String,
    /// The PM thread of the ticket the relay is about; the primary PM when absent.
    #[serde(default)]
    pm: Option<String>,
}

/// The payload of a `notice` effect; its key names what it reports.
#[derive(Debug, Deserialize)]
struct NoticePayload {
    value: Value,
    summary: String,
    /// The PM thread the notice is for; the primary PM when absent.
    #[serde(default)]
    pm: Option<String>,
}

impl PmRuntime {
    fn primary_thread(&self) -> Result<ThreadId, EffectError> {
        let Some(id) = self.pm_threads.primary() else {
            return Err(EffectError::Later(format!(
                "no PM thread registered in {}",
                self.pm_threads.path().display()
            )));
        };
        ThreadId::from_string(&id)
            .map_err(|err| EffectError::Failed(format!("PM thread id {id} is invalid: {err}")))
    }

    /// The PM a delivery is for: the thread that owns the conversation when it is a registered PM
    /// (one org, several PMs, one project each), else the primary PM.
    fn pm_for(&self, preferred: Option<&str>) -> Result<ThreadId, EffectError> {
        if let Some(id) = preferred
            && self.pm_threads.is_pm(id)
        {
            return ThreadId::from_string(id).map_err(|err| {
                EffectError::Failed(format!("PM thread id {id} is invalid: {err}"))
            });
        }
        self.primary_thread()
    }

    /// Candidate cards are steered into the PM's running turn, so the turn that reads the person's
    /// message decides them at once and no stale card follows a PM that routed early; relays and
    /// notices wait for the next turn.
    async fn deliver_to_pm_for(
        &self,
        pm: Option<&str>,
        text: String,
        mode: DeliveryMode,
    ) -> Result<(), EffectError> {
        let thread = self.pm_for(pm)?;
        self.host
            .deliver(thread, text, mode)
            .await
            .map_err(|err| EffectError::Failed(format!("{err:#}")))
    }

    /// Renders the candidate the Eval holds and hands it to the PM, then tells the engine.
    pub async fn deliver_candidate(
        &self,
        eval_id: &str,
        trigger_id: &str,
    ) -> Result<(), EffectError> {
        let candidate = self
            .engine
            .candidate(trigger_id.to_owned())
            .await
            .map_err(|err| EffectError::Failed(err.to_string()))?;
        // The Eval ended or another replaced it: nothing to deliver.
        let Some(candidate) = candidate.filter(|c| c.eval_id == eval_id) else {
            return Ok(());
        };
        let text = self
            .render_candidate(&candidate)
            .await
            .map_err(EffectError::from)?;
        // The card goes to the PM whose conversation carried the message (the primary Trigger
        // is last in the batch).
        let pm = candidate
            .triggers
            .last()
            .and_then(|t| t.channel.as_ref())
            .map(|c| c.thread.clone());
        self.deliver_to_pm_for(pm.as_deref(), text, DeliveryMode::Steer)
            .await?;
        self.engine
            .candidate_delivered(eval_id.to_owned())
            .await
            .map_err(|err| EffectError::Failed(err.to_string()))
    }

    pub async fn render_candidate(&self, candidate: &Candidate) -> anyhow::Result<String> {
        let mut dispositions: Vec<(usize, Option<TriggerDisposition>)> = Vec::new();
        for (index, trigger) in candidate.triggers.iter().enumerate() {
            dispositions.push((index, self.store.disposition(&trigger.id).await?));
        }
        let primary_text = candidate
            .triggers
            .last()
            .map(|t| t.text.as_str())
            .unwrap_or_default();
        let mut referenced = Vec::new();
        for ticket in self.store.tickets().await? {
            if candidate.tickets.iter().any(|t| t.id == ticket.id) {
                continue;
            }
            if primary_text.contains(&ticket.id) {
                referenced.push(ticket);
            }
        }
        // The tickets that handed a batch message back, for the context the PM passes on.
        let mut origins: Vec<crate::core::Ticket> = Vec::new();
        for (_, disposition) in &dispositions {
            let Some(disposition) = disposition else {
                continue;
            };
            for handback in disposition.handbacks.iter().flatten() {
                if origins.iter().any(|t| t.id == handback.ticket_id) {
                    continue;
                }
                if let Some(ticket) = self.store.ticket(&handback.ticket_id).await? {
                    origins.push(ticket);
                }
            }
        }
        let mut origin_texts = std::collections::HashMap::new();
        for ticket in &origins {
            if let Some(trigger) = self.store.trigger(&ticket.origin.trigger_id).await? {
                origin_texts.insert(
                    ticket.id.clone(),
                    trigger.text.chars().take(300).collect::<String>(),
                );
            }
        }
        let tickets_total = self.store.tickets().await?.len();
        let mode = serde_json::to_value(candidate.mode)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| "observe".to_owned());
        let context = CandidateContext {
            dispositions: dispositions
                .iter()
                .map(|(index, d)| (&candidate.triggers[*index], d.as_ref()))
                .collect(),
            referenced: referenced.iter().collect(),
            tickets_total,
            mode: &mode,
            origins: origins.iter().collect(),
            origin_texts,
        };
        Ok(candidate_card(
            &candidate.eval_id,
            &candidate.triggers,
            candidate.strength,
            &candidate.tickets,
            &context,
        ))
    }
}

/// Registers the handlers of the `assessment`, `relay` and `notice` effects on the queue.
pub async fn install_pm_handlers(runtime: Arc<PmRuntime>) -> anyhow::Result<()> {
    let effects = runtime.effects.clone();
    let assessment: EffectRun = {
        let runtime = Arc::clone(&runtime);
        Arc::new(move |payload: Value, eval_id: String| {
            let runtime = Arc::clone(&runtime);
            Box::pin(async move {
                let payload: AssessmentPayload = serde_json::from_value(payload)
                    .map_err(|err| EffectError::Failed(format!("assessment payload: {err}")))?;
                runtime
                    .deliver_candidate(&eval_id, &payload.trigger_id)
                    .await
            })
        })
    };
    effects
        .handle(
            "assessment",
            assessment,
            HandlerOptions::default()
                .limit(|| Ok(4))
                .retry(2_000, 60_000),
        )
        .await?;
    let relay: EffectRun = {
        let runtime = Arc::clone(&runtime);
        Arc::new(move |payload: Value, _key: String| {
            let runtime = Arc::clone(&runtime);
            Box::pin(async move {
                let payload: RelayPayload = serde_json::from_value(payload)
                    .map_err(|err| EffectError::Failed(format!("relay payload: {err}")))?;
                runtime
                    .deliver_to_pm_for(payload.pm.as_deref(), payload.text, DeliveryMode::Queue)
                    .await
            })
        })
    };
    effects
        .handle(
            "relay",
            relay,
            HandlerOptions::default().ordered().retry(2_000, 60_000),
        )
        .await?;
    let notice: EffectRun = {
        let runtime = Arc::clone(&runtime);
        Arc::new(move |payload: Value, name: String| {
            let runtime = Arc::clone(&runtime);
            Box::pin(async move {
                let payload: NoticePayload = serde_json::from_value(payload)
                    .map_err(|err| EffectError::Failed(format!("notice payload: {err}")))?;
                let pm = match &payload.pm {
                    Some(pm) => Some(pm.clone()),
                    None => match payload
                        .value
                        .get("ticketId")
                        .or_else(|| payload.value.get("ticket_id"))
                        .and_then(Value::as_str)
                    {
                        Some(ticket_id) => runtime
                            .store
                            .ticket(ticket_id)
                            .await
                            .ok()
                            .flatten()
                            .and_then(|t| t.channels.first().map(|c| c.thread.clone())),
                        None => None,
                    },
                };
                let text = json!({ "kind": "notice", "name": name, "summary": payload.summary, "notice": payload.value }).to_string();
                runtime
                    .deliver_to_pm_for(pm.as_deref(), text, DeliveryMode::Queue)
                    .await
            })
        })
    };
    effects
        .handle(
            "notice",
            notice,
            HandlerOptions::default().ordered().retry(2_000, 60_000),
        )
        .await?;
    Ok(())
}

/// Every tool a Router thread can be given at any level (`router.rs`); the host's tool policy
/// for PM threads allows exactly these, so a tool missing here never reaches the model.
pub const ROUTER_TOOLS: &[&str] = &[
    crate::tools::read::READ_TOOL,
    crate::tools::pm::ROUTE_TOOL,
    crate::tools::pm::TICKET_NOTE_TOOL,
    crate::tools::pm::TICKET_RETRY_TOOL,
    crate::tools::pm::REQUEST_STATUS_TOOL,
    crate::tools::pm::TRIGGER_REQUEUE_TOOL,
    crate::tools::drive::DRIVE_TOOL,
];

/// The tools a PM thread gets.
pub fn pm_tools(
    runtime: &Arc<PmRuntime>,
) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
    let mut tools: Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> = Vec::new();
    tools.push(Arc::new(ReadTool::for_pm(
        runtime.store.clone(),
        Arc::clone(&runtime.host),
    )));
    for kind in [
        PmToolKind::Route,
        PmToolKind::TicketNote,
        PmToolKind::TicketRetry,
        PmToolKind::RequestStatus,
        PmToolKind::TriggerRequeue,
    ] {
        tools.push(Arc::new(PmTool::new(kind, Arc::clone(runtime))));
    }
    tools
}

/// The extension contribution: PM threads get the tools of the Router's level (`router.rs`),
/// other threads nothing from here.
pub struct PmExtension {
    service: Arc<SuperagentService>,
}

impl PmExtension {
    pub fn new(service: Arc<SuperagentService>) -> Self {
        Self { service }
    }
}

impl ToolContributor for PmExtension {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
        let Some(runtime) = self.service.pm() else {
            return Vec::new();
        };
        if !runtime.pm_threads.is_pm(thread_store.level_id()) {
            return Vec::new();
        }
        match self.service.router_mode() {
            // The observing Router reads; nothing it can call sends or opens work.
            RouterMode::Observe => vec![Arc::new(ReadTool::for_pm(
                runtime.store.clone(),
                Arc::clone(&runtime.host),
            ))],
            // Driving reads and drives; it opens no tickets.
            RouterMode::Drive => vec![
                Arc::new(ReadTool::for_pm(
                    runtime.store.clone(),
                    Arc::clone(&runtime.host),
                )),
                Arc::new(DriveTool::new(
                    crate::drive_lookup(&self.service),
                    Some(thread_store.level_id().to_owned()),
                )),
            ],
            RouterMode::Org => {
                let mut tools = pm_tools(&runtime);
                tools.push(Arc::new(DriveTool::new(
                    crate::drive_lookup(&self.service),
                    Some(thread_store.level_id().to_owned()),
                )));
                tools
            }
        }
    }
}
