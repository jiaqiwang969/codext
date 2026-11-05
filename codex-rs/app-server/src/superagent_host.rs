//! The Superagent engine's host (design §1.2): what the executor asks app-server to do with codext
//! threads, over the same services the request processors use.
//!
//! Implemented here: start and resume (the thread processor's `start_thread_internal` and
//! `resume_thread_with`, which run the client paths without a client), deliver (the persistent
//! queue, or `start_or_steer_turn`), interrupt, background commands (list and clean), children (the
//! agent graph), goals (set, clear, status) and verified account selection. Profile changes
//! use the same idle checks and runtime replacement as an explicit client profile switch.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Weak;

use serde_json::Value as JsonValue;

use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_core::TurnInput;
use codex_core::TurnInputRequest;
use codex_goal_extension::GoalObjectiveUpdate;
use codex_goal_extension::GoalService;
use codex_goal_extension::GoalSetRequest;
use codex_goal_extension::GoalTokenBudgetUpdate;
use codex_protocol::ThreadId;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_protocol::user_input::UserInput;
use codex_queue_extension::QueuedItemService;
use codex_state::StateRuntime;
use codex_superagent_extension::BoxFut;
use codex_superagent_extension::host::DeliveryMode;
use codex_superagent_extension::host::GoalStatus;
use codex_superagent_extension::host::SessionBrief;
use codex_superagent_extension::host::SessionPage;
use codex_superagent_extension::host::SuperagentHost;
use codex_superagent_extension::host::ThreadSpec;
use codex_superagent_extension::host::TurnBrief;

use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadStartParams;
use codex_extension_api::ToolName;
use codex_extension_api::ToolPolicy;
use std::sync::OnceLock;

use crate::request_processors::ThreadRequestProcessor;
use crate::request_processors::TurnRequestProcessor;

pub(crate) struct AppServerSuperagentHost {
    pub(crate) thread_manager: Weak<ThreadManager>,
    pub(crate) queue_service: Option<Arc<QueuedItemService>>,
    pub(crate) goal_service: Arc<GoalService>,
    pub(crate) state_db: Arc<StateRuntime>,
    /// The loaded config: worker role files (`agents/<role>.toml`) are resolved through it.
    pub(crate) config: Arc<Config>,
    /// The thread processor, once built: it starts and resumes threads for the engine.
    pub(crate) thread_processor: OnceLock<ThreadRequestProcessor>,
    /// The turn processor, once built: thread settings (model, effort) for the capacity policy.
    pub(crate) turn_processor: OnceLock<TurnRequestProcessor>,
}

/// What a PM thread may call: the Router's superagent tools and the plan tool, nothing that
/// dispatches work natively (design decision #7). Captured by core when the thread starts or
/// resumes.
fn pm_tool_policy() -> ToolPolicy {
    ToolPolicy {
        allowed_tools: Some(
            codex_superagent_extension::pm::ROUTER_TOOLS
                .iter()
                .copied()
                .chain(["update_plan"])
                .map(ToolName::plain)
                .collect(),
        ),
        ..ToolPolicy::default()
    }
}

/// Apply the PM policy at creation, cold resume, and account runtime replacement.
pub(crate) fn thread_tool_policy(
    service: Option<&codex_superagent_extension::SuperagentService>,
    thread_id: ThreadId,
) -> Option<ToolPolicy> {
    service.filter(|service| service.pm_threads().is_pm(&thread_id.to_string()))
        .map(|_| pm_tool_policy())
}

impl AppServerSuperagentHost {
    pub(crate) fn set_thread_processor(&self, processor: ThreadRequestProcessor) {
        let _ = self.thread_processor.set(processor);
    }

    pub(crate) fn set_turn_processor(&self, processor: TurnRequestProcessor) {
        let _ = self.turn_processor.set(processor);
    }

    fn processor(&self) -> anyhow::Result<&ThreadRequestProcessor> {
        self.thread_processor
            .get()
            .ok_or_else(|| anyhow::anyhow!("the thread processor is not ready yet"))
    }

    fn thread_manager(&self) -> anyhow::Result<Arc<ThreadManager>> {
        self.thread_manager
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("the thread manager is gone"))
    }

    async fn thread(&self, thread: ThreadId) -> anyhow::Result<Arc<codex_core::CodexThread>> {
        self.thread_manager()?
            .get_thread(thread)
            .await
            .map_err(|err| anyhow::anyhow!("thread {thread} is not loaded: {err}"))
    }

    fn user_input(text: String) -> TurnInput {
        TurnInput::UserInput {
            content: vec![UserInput::Text {
                text,
                text_elements: Vec::new(),
            }],
            client_id: None,
        }
    }
}

impl SuperagentHost for AppServerSuperagentHost {
    fn list_sessions(
        &self,
        cursor: Option<String>,
        limit: u32,
    ) -> BoxFut<'_, anyhow::Result<SessionPage>> {
        Box::pin(async move {
            let response = self
                .processor()?
                .list_sessions_internal(cursor, limit)
                .await
                .map_err(|err| anyhow::anyhow!("listing sessions failed: {}", err.message))?;
            let value = serde_json::to_value(&response)?;
            let text = |value: &JsonValue, key: &str| {
                value
                    .get(key)
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned)
            };
            let sessions = value
                .get("data")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
                .map(|thread| SessionBrief {
                    id: text(thread, "id").unwrap_or_default(),
                    name: text(thread, "name"),
                    preview: text(thread, "preview").unwrap_or_default(),
                    cwd: text(thread, "cwd").unwrap_or_default(),
                    // `active:waitingOnApproval` when the session waits for the person.
                    status: thread
                        .get("status")
                        .map(|status| {
                            let kind = text(status, "type").unwrap_or_default();
                            let flags: Vec<&str> = status
                                .get("activeFlags")
                                .and_then(JsonValue::as_array)
                                .into_iter()
                                .flatten()
                                .filter_map(JsonValue::as_str)
                                .collect();
                            if flags.is_empty() {
                                kind
                            } else {
                                format!("{kind}:{}", flags.join(","))
                            }
                        })
                        .unwrap_or_default(),
                    updated_at: thread.get("updatedAt").and_then(JsonValue::as_i64),
                    model: text(thread, "model"),
                    effort: text(thread, "reasoningEffort"),
                    account_profile: thread
                        .get("extra")
                        .and_then(|extra| text(extra, "accountProfile")),
                })
                .collect();
            Ok(SessionPage {
                sessions,
                next_cursor: text(&value, "nextCursor"),
            })
        })
    }

    fn thread_profile(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<Option<String>>> {
        Box::pin(async move {
            Ok(self
                .thread_manager()?
                .auth_profile_id_for_thread(thread)
                .await)
        })
    }

    fn session_turns(
        &self,
        thread: ThreadId,
        limit: u32,
    ) -> BoxFut<'_, anyhow::Result<Vec<TurnBrief>>> {
        Box::pin(async move {
            let response = self
                .processor()?
                .session_turns_internal(thread.to_string(), limit)
                .await
                .map_err(|err| {
                    anyhow::anyhow!("reading turns of {thread} failed: {}", err.message)
                })?;
            let value = serde_json::to_value(&response)?;
            let turns = value
                .get("data")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
                .map(|turn| {
                    let mut brief = TurnBrief {
                        id: turn
                            .get("id")
                            .and_then(JsonValue::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        status: turn
                            .get("status")
                            .and_then(JsonValue::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        ..TurnBrief::default()
                    };
                    for item in turn
                        .get("items")
                        .and_then(JsonValue::as_array)
                        .into_iter()
                        .flatten()
                    {
                        match item.get("type").and_then(JsonValue::as_str) {
                            Some("userMessage") => brief.user.extend(
                                item.get("content")
                                    .and_then(JsonValue::as_array)
                                    .into_iter()
                                    .flatten()
                                    .filter_map(|input| {
                                        input.get("text").and_then(JsonValue::as_str)
                                    })
                                    .map(str::to_owned),
                            ),
                            Some("agentMessage") => brief.assistant.extend(
                                item.get("text")
                                    .and_then(JsonValue::as_str)
                                    .map(str::to_owned),
                            ),
                            _ => {}
                        }
                    }
                    brief
                })
                .collect();
            Ok(turns)
        })
    }

    fn queued(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<(String, String)>>> {
        Box::pin(async move {
            let Some(queue) = self.queue_service.as_ref() else {
                return Ok(Vec::new());
            };
            let items = queue.list(thread).await?;
            Ok(items
                .into_iter()
                .map(|item| {
                    let text = match item.input {
                        TurnInput::UserInput { content, .. } => content
                            .into_iter()
                            .filter_map(|input| match input {
                                UserInput::Text { text, .. } => Some(text),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                        _ => String::new(),
                    };
                    (item.id, text)
                })
                .collect())
        })
    }

    fn withdraw_queued(
        &self,
        thread: ThreadId,
        item_id: String,
    ) -> BoxFut<'_, anyhow::Result<bool>> {
        Box::pin(async move {
            let Some(queue) = self.queue_service.as_ref() else {
                return Ok(false);
            };
            Ok(queue.delete(thread, item_id).await?)
        })
    }

    fn start_thread(&self, spec: ThreadSpec) -> BoxFut<'_, anyhow::Result<ThreadId>> {
        Box::pin(async move {
            let ThreadSpec {
                cwd,
                profile,
                model,
                role,
                developer_instructions,
            } = spec;
            // The role file (`agents/<role>.toml` in any config layer) fills what the spec leaves
            // open: the model, the developer instructions and the reasoning effort. Without it a
            // worker would inherit the PM's instructions from the shared config.
            let role = match role.as_deref() {
                Some(name) => self.role_layer(name).await?,
                None => RoleLayer::default(),
            };
            let mut overrides = HashMap::new();
            if let Some(effort) = role.model_reasoning_effort {
                overrides.insert(
                    "model_reasoning_effort".to_owned(),
                    JsonValue::String(effort),
                );
            }
            let params = ThreadStartParams {
                cwd: Some(cwd.to_string_lossy().into_owned()),
                model: model.or(role.model),
                developer_instructions: developer_instructions.or(role.developer_instructions),
                config: (!overrides.is_empty()).then_some(overrides),
                ephemeral: Some(false),
                ..ThreadStartParams::default()
            };
            self.processor()?
                .start_thread_internal(params, profile)
                .await
                .map_err(|err| {
                    anyhow::anyhow!("thread/start for the worker failed: {}", err.message)
                })
        })
    }

    fn bind_profile(&self, thread: ThreadId, profile: String) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async move {
            if self.thread_manager()?.auth_profile_id_for_thread(thread).await.as_deref()
                == Some(profile.as_str())
            {
                return Ok(());
            }
            let session = crate::message_processor::ConnectionSessionState::new(
                crate::transport::ConnectionOrigin::InProcess,
            );
            let request_id = ThreadRequestProcessor::superagent_request_id(&format!("profile-{thread}"));
            self.processor()?.switch_auth_profile(
                &request_id,
                codex_app_server_protocol::SwitchAuthProfileParams {
                    name: profile.clone(),
                    thread_id: Some(thread.to_string()),
                },
                &session,
                /*app_server_client_name*/ None,
                /*app_server_client_version*/ None,
            ).await.map_err(|error| anyhow::anyhow!("profile switch for {thread}: {}", error.message))?;
            if self.thread(thread).await.is_ok()
                && self.thread_manager()?.auth_profile_id_for_thread(thread).await.as_deref() != Some(profile.as_str())
            {
                anyhow::bail!("the loaded worker {thread} did not select profile {profile}");
            }
            Ok(())
        })
    }

    fn resume_thread(
        &self,
        thread: ThreadId,
        role: Option<String>,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async move {
            if self.thread(thread).await.is_ok() {
                return Ok(());
            }
            // A cold load follows the role file as it is now: the model, the effort and the
            // developer instructions of a resumed worker are the current ones, not the ones
            // the thread was created with. Without a role nothing is overridden.
            let role = match role.as_deref() {
                Some(name) => self.role_layer(name).await?,
                None => RoleLayer::default(),
            };
            let mut overrides = HashMap::new();
            if let Some(effort) = role.model_reasoning_effort {
                overrides.insert(
                    "model_reasoning_effort".to_owned(),
                    JsonValue::String(effort),
                );
            }
            let params = ThreadResumeParams {
                thread_id: thread.to_string(),
                exclude_turns: true,
                model: role.model,
                developer_instructions: role.developer_instructions,
                config: (!overrides.is_empty()).then_some(overrides),
                ..ThreadResumeParams::default()
            };
            self.processor()?
                .resume_thread_with(params, /*auth_profile_id*/ None)
                .await
                .map_err(|err| {
                    anyhow::anyhow!("thread/resume for {thread} failed: {}", err.message)
                })
        })
    }

    fn set_thread_model(
        &self,
        thread: ThreadId,
        role: Option<String>,
        model: Option<String>,
        effort: Option<String>,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async move {
            // nothing explicit: back to the role file's values
            let (model, effort) = match (model, effort) {
                (None, None) => match role.as_deref() {
                    Some(name) => {
                        let role = self.role_layer(name).await?;
                        (role.model, role.model_reasoning_effort)
                    }
                    None => (None, None),
                },
                pair => pair,
            };
            let params = serde_json::from_value(serde_json::json!({
                "threadId": thread.to_string(),
                "model": model,
                "effort": effort,
            }))
            .map_err(|err| anyhow::anyhow!("thread settings for {thread}: {err}"))?;
            let processor = self
                .turn_processor
                .get()
                .ok_or_else(|| anyhow::anyhow!("the turn processor is not ready yet"))?;
            let request_id =
                ThreadRequestProcessor::superagent_request_id(&format!("model-{thread}"));
            processor
                .thread_settings_update(&request_id, params)
                .await
                .map(|_| ())
                .map_err(|err| {
                    anyhow::anyhow!(
                        "thread/settings/update for {thread} failed: {}",
                        err.message
                    )
                })
        })
    }

    fn deliver(
        &self,
        thread: ThreadId,
        text: String,
        mode: DeliveryMode,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async move {
            // A cold thread takes nothing: load it first (a no-op while it is loaded).
            self.resume_thread(thread, None).await?;
            let input = Self::user_input(text);
            match mode {
                DeliveryMode::Queue => {
                    let queue = self
                        .queue_service
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("the persistent queue is off"))?;
                    queue.enqueue(thread, input).await?;

                }
                DeliveryMode::Steer => {
                    self.thread(thread)
                        .await?
                        .start_or_steer_turn(TurnInputRequest::new(input))
                        .await
                        .map_err(|err| anyhow::anyhow!("steer into {thread} failed: {err}"))?;
                }
            }
            Ok(())
        })
    }

    fn start_step(&self, thread: ThreadId, text: String) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async move {
            // A cold session loads with the config's defaults; the step's own settings below are
            // what its turn runs under.
            self.resume_thread(thread, None).await?;
            let mut request = TurnInputRequest::new(Self::user_input(text));
            // Full access, as the person decided on 2026-11-05: the worker may open applications
            // and browsers; staying inside its folder is a rule it is told, not a sandbox.
            request.thread_settings.sandbox_policy =
                Some(codex_protocol::protocol::SandboxPolicy::DangerFullAccess);
            request.thread_settings.approval_policy =
                Some(codex_protocol::protocol::AskForApproval::Never);
            request.start.turn_trigger = Some("drive".to_owned());
            self.thread(thread)
                .await?
                .start_or_steer_turn(request)
                .await
                .map_err(|err| {
                    anyhow::anyhow!("starting a drive step on {thread} failed: {err}")
                })?;
            Ok(())
        })
    }

    fn interrupt(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async move {
            self.thread(thread)
                .await?
                .submit(Op::Interrupt)
                .await
                .map_err(|err| anyhow::anyhow!("interrupt of {thread} failed: {err}"))?;
            Ok(())
        })
    }

    fn clean_background(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async move {
            self.thread(thread)
                .await?
                .submit(Op::CleanBackgroundTerminals)
                .await
                .map_err(|err| {
                    anyhow::anyhow!("cleaning the background commands of {thread} failed: {err}")
                })?;
            Ok(())
        })
    }

    fn background_commands(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<String>>> {
        Box::pin(async move {
            let Ok(thread) = self.thread(thread).await else {
                return Ok(Vec::new());
            };
            let running = thread.list_background_terminals().await;
            Ok((0..running.len())
                .map(|index| format!("background terminal {}", index + 1))
                .collect())
        })
    }

    fn children(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<ThreadId>>> {
        Box::pin(async move {
            let manager = self.thread_manager()?;
            let children = manager
                .list_agent_subtree_thread_ids(thread)
                .await
                .map_err(|err| anyhow::anyhow!("listing the children of {thread} failed: {err}"))?;
            let mut running = Vec::new();
            for child in children {
                if child != thread
                    && let Ok(child_thread) = manager.get_thread(child).await
                    && matches!(
                        child_thread.agent_status().await,
                        codex_protocol::protocol::AgentStatus::Running
                    )
                {
                    running.push(child);
                }
            }
            Ok(running)
        })
    }

    fn goal_set(
        &self,
        thread: ThreadId,
        objective: String,
        token_budget: Option<i64>,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async move {
            self.goal_service
                .set_thread_goal(
                    &self.state_db,
                    GoalSetRequest {
                        thread_id: thread,
                        objective: GoalObjectiveUpdate::Set(&objective),
                        status: None,
                        token_budget: GoalTokenBudgetUpdate::Set(token_budget),
                        max_goal_token_budget: None,
                    },
                    async { Ok(()) },
                )
                .await
                .map_err(|err| anyhow::anyhow!("setting the goal of {thread} failed: {err}"))?;
            Ok(())
        })
    }

    fn tokens_used(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<Option<i64>>> {
        Box::pin(async move {
            let metadata = self.state_db.get_thread(thread).await.map_err(|err| {
                anyhow::anyhow!("reading the token count of {thread} failed: {err}")
            })?;
            Ok(metadata.map(|metadata| metadata.tokens_used))
        })
    }

    fn goal_clear(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async move {
            self.goal_service
                .clear_thread_goal(&self.state_db, thread, async { Ok(()) })
                .await
                .map_err(|err| anyhow::anyhow!("clearing the goal of {thread} failed: {err}"))?;
            Ok(())
        })
    }

    fn goal_status(&self, thread: ThreadId) -> BoxFut<'_, anyhow::Result<Option<GoalStatus>>> {
        Box::pin(async move {
            let goal = self
                .goal_service
                .get_thread_goal(&self.state_db, thread)
                .await
                .map_err(|err| anyhow::anyhow!("reading the goal of {thread} failed: {err}"))?;
            Ok(goal.map(|goal| match goal.status {
                ThreadGoalStatus::Active => GoalStatus::Active,
                ThreadGoalStatus::Complete => GoalStatus::Completed,
                ThreadGoalStatus::BudgetLimited => GoalStatus::BudgetLimited,
                ThreadGoalStatus::UsageLimited => GoalStatus::UsageLimited,
                ThreadGoalStatus::Paused => GoalStatus::Other("paused".to_owned()),
                ThreadGoalStatus::Blocked => GoalStatus::Other("blocked".to_owned()),
            }))
        })
    }
}

/// What a role file sets for a worker thread.
#[derive(Default)]
struct RoleLayer {
    model: Option<String>,
    model_reasoning_effort: Option<String>,
    developer_instructions: Option<String>,
}

impl AppServerSuperagentHost {
    /// The role's file as `[agents]` discovered it (user, project or managed layer): a flat TOML
    /// whose `model`, `model_reasoning_effort` and `developer_instructions` apply to the thread.
    async fn role_layer(&self, name: &str) -> anyhow::Result<RoleLayer> {
        let Some(role) = self.config.agent_roles.get(name) else {
            anyhow::bail!("unknown worker role `{name}`: no agents/{name}.toml in any config layer");
        };
        let Some(file) = role.config_file.as_ref() else {
            return Ok(RoleLayer::default());
        };
        let text = tokio::fs::read_to_string(file)
            .await
            .map_err(|err| anyhow::anyhow!("reading the role file {}: {err}", file.display()))?;
        let table: toml::Value = toml::from_str(&text)
            .map_err(|err| anyhow::anyhow!("parsing the role file {}: {err}", file.display()))?;
        let string = |key: &str| table.get(key).and_then(toml::Value::as_str).map(str::to_owned);
        Ok(RoleLayer {
            model: string("model"),
            model_reasoning_effort: string("model_reasoning_effort"),
            developer_instructions: string("developer_instructions"),
        })
    }
}
