//! Rebind an idle conversation to an account without changing shared credentials.

use super::*;
use crate::message_processor::ConnectionSessionState;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::SessionSource;

impl ThreadRequestProcessor {
    pub(crate) async fn switch_auth_profile(
        &self,
        request_id: &ConnectionRequestId,
        params: SwitchAuthProfileParams,
        session: &ConnectionSessionState,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        // Use the same admission lock as turn/start and resume. No new client
        // turn can capture the old runtime while the replacement is installed.
        let _auth_transition_guard = self.auth_transition_lock.lock().await;
        let auth_manager = self
            .account_profiles
            .auth_manager_for_profile(&params.name)
            .await
            .map_err(invalid_params)?;
        let mut credentials_changed = false;
        let running_turns = *self
            .thread_watch_manager
            .subscribe_running_turn_count()
            .borrow();
        if running_turns == 0 {
            match auth_manager.reload_with_status().await {
                codex_login::AuthReloadStatus::Failed => {
                    return Err(invalid_params(format!(
                        "failed to reload authentication profile `{}` from storage",
                        params.name
                    )));
                }
                codex_login::AuthReloadStatus::Reloaded { changed } => {
                    credentials_changed = changed;
                    if changed {
                        self.thread_manager
                            .invalidate_model_transport_caches_for_auth(&auth_manager)
                            .await;
                    }
                }
            }
            if auth_manager.auth_cached().is_none() {
                return Err(invalid_params(format!(
                    "authentication profile `{}` has no usable credentials",
                    params.name
                )));
            }
        }

        let mut changed = credentials_changed
            || session.auth_profile_id().await.as_deref() != Some(params.name.as_str());
        if let Some(thread_id) = params.thread_id.as_deref() {
            let _thread_list_state_permit = self.acquire_thread_list_state_permit().await?;
            let thread_id = ThreadId::from_string(thread_id)
                .map_err(|error| invalid_request(format!("invalid thread id: {error}")))?;
            if self
                .pending_thread_unloads
                .lock()
                .await
                .contains_key(&thread_id)
            {
                return Err(invalid_request(format!(
                    "thread {thread_id} is closing; retry the account switch after it is closed"
                )));
            }
            let _goal_resume_guard = self.thread_state_manager.lock_goal_resume(thread_id).await;
            // Shutdown also owns the local agent tree. Never interrupt active
            // work implicitly just because an account was selected in a picker.
            for id in self
                .thread_manager
                .list_agent_subtree_thread_ids(thread_id)
                .await
                .map_err(|error| internal_error(error.to_string()))?
            {
                if let Ok(agent) = self.thread_manager.get_thread(id).await
                    && matches!(agent.agent_status().await, AgentStatus::Running)
                {
                    return Err(invalid_request(
                        "wait for the current task and its agents to finish, or interrupt them before switching accounts",
                    ));
                }
            }
            if let Ok(thread) = self.thread_manager.get_thread(thread_id).await {
                ensure_direct_input_allowed(thread.as_ref()).await?;
                if thread.config_snapshot().await.ephemeral {
                    return Err(invalid_request(
                        "account switching requires a persistent thread",
                    ));
                }
                let current_profile = self
                    .thread_manager
                    .auth_profile_id_for_thread(thread_id)
                    .await;
                if current_profile.as_deref() != Some(params.name.as_str()) {
                    self.replace_thread_account_runtime(
                        request_id,
                        thread_id,
                        thread,
                        &params.name,
                        app_server_client_name,
                        app_server_client_version,
                    )
                    .await?;
                    changed = true;
                }
            } else {
                // Select the account before cold resume loads any runtime or
                // continues a goal. The previous account need not exist or log in.
                let stored_thread = self
                    .read_stored_thread_for_resume(
                        &thread_id.to_string(),
                        /*path*/ None,
                        /*include_history*/ false,
                    )
                    .await?;
                if matches!(stored_thread.source, SessionSource::SubAgent(_)) {
                    let (history, _) = self
                        .load_resume_initial_history_from_stored_thread(
                            stored_thread,
                            /*prepared_model_context*/ None,
                        )
                        .await?;
                    if let Some((source, _)) = history.get_resumed_session_sources()
                        && !can_accept_direct_input(history.get_multi_agent_version(), &source)
                    {
                        return Err(invalid_request(
                            "switch a multi-agent v2 sub-agent through its parent session",
                        ));
                    }
                }
                changed |= self
                    .account_profiles
                    .thread_profile(thread_id)
                    .map_err(internal_error)?
                    .as_deref()
                    != Some(params.name.as_str());
            }
            // Acknowledge only after any live runtime and its next cold resume
            // select this account. The creator identity remains historical.
            self.account_profiles
                .save_thread_profile(thread_id, &params.name)
                .map_err(internal_error)?;
            tracing::info!(%thread_id, profile = %params.name, "switched thread authentication profile");
        }
        session.set_auth_profile_id(Some(params.name.clone())).await;
        let auth = auth_manager.auth_cached();
        self.outgoing
            .send_server_notification_to_connections(
                &[request_id.connection_id],
                ServerNotification::AccountUpdated(AccountUpdatedNotification {
                    auth_mode: auth
                        .as_ref()
                        .map(CodexAuth::api_auth_mode)
                        .map(auth_mode_to_api),
                    plan_type: auth.as_ref().and_then(CodexAuth::account_plan_type),
                }),
            )
            .await;
        Ok(Some(
            SwitchAuthProfileResponse {
                name: params.name,
                changed,
                thread_id: params.thread_id,
            }
            .into(),
        ))
    }

    async fn replace_thread_account_runtime(
        &self,
        request_id: &ConnectionRequestId,
        thread_id: ThreadId,
        thread: Arc<CodexThread>,
        profile: &str,
        app_server_client_name: Option<String>,
        app_server_client_version: Option<String>,
    ) -> Result<(), JSONRPCErrorError> {
        let settings = thread.restorable_thread_settings().await;
        let mut config = thread.config().await.as_ref().clone();
        if let Some(mode) = settings.collaboration_mode.as_ref() {
            config.model = Some(mode.model().to_owned());
        }
        if let Some(service_tier) = settings.service_tier.as_ref() {
            config.service_tier.clone_from(service_tier);
        }
        config.personality = settings.personality;
        let ema_cancellation = config
            .config_layer_stack
            .mcp_ema_cancellation()
            .for_startup();
        config.config_layer_stack = config
            .config_layer_stack
            .with_mcp_ema_cancellation(ema_cancellation);
        let runtime = self
            .account_profiles
            .runtime_for_profile(&self.thread_manager, &config, Some(profile))
            .await
            .map_err(invalid_params)?;
        let client_mcp_extensions = thread.client_mcp_extensions();
        // The Superagent engine is not a client: its requests never attach a subscriber, but the
        // listener the switch drains is started all the same.
        if matches!(
            self.ensure_conversation_listener(
                thread_id,
                request_id.connection_id,
                /*raw_events_enabled*/ false,
            )
            .await?,
            EnsureConversationListenerResult::ConnectionClosed
        ) && request_id.connection_id != super::superagent::SUPERAGENT_CONNECTION
        {
            return Err(internal_error("connection closed before account switch"));
        }
        let thread_state = self.thread_state_manager.thread_state(thread_id).await;
        let mut shutdown_drain = thread_state
            .lock()
            .await
            .shutdown_drain
            .clone()
            .ok_or_else(|| {
                internal_error("the previous account runtime has no shutdown listener")
            })?;
        if !matches!(
            wait_for_thread_shutdown(&thread).await,
            ThreadShutdownResult::Complete
        ) {
            return Err(internal_error(
                "could not shut down the previous account runtime",
            ));
        }
        tokio::time::timeout(
            Duration::from_secs(10),
            shutdown_drain.wait_for(|drained| *drained),
        )
        .await
        .map_err(|_| internal_error("timed out draining the previous account runtime"))?
        .map_err(|_| internal_error("the previous account listener stopped before draining"))?;
        self.thread_manager
            .remove_thread_if_matches(&thread_id, &thread)
            .await;
        self.outgoing
            .cancel_requests_for_thread(thread_id, /*error*/ None)
            .await;

        let stored_thread = self
            .read_stored_thread_for_resume(
                &thread_id.to_string(),
                /*path*/ None,
                /*include_history*/ false,
            )
            .await?;
        let (initial_history, _) = self
            .load_resume_initial_history_from_stored_thread(
                stored_thread,
                /*prepared_model_context*/ None,
            )
            .await?;
        let mut thread_extension_init = codex_extension_api::ExtensionDataInit::new();
        if let Some(policy) =
            crate::superagent_host::thread_tool_policy(self.superagent.as_deref(), thread_id)
        {
            thread_extension_init.insert(policy);
        }
        let NewThread {
            thread: replacement,
            ..
        } = self
            .thread_manager
            .start_thread(StartThreadOptions {
                runtime,
                thread_extension_init,
                initial_history,
                client_mcp_extensions,
                parent_trace: self.request_trace_context(request_id).await,
                ..StartThreadOptions::new(config)
            })
            .await
            .map_err(|error| {
                internal_error(format!("failed to start selected account runtime: {error}"))
            })?;
        replacement
            .restore_thread_settings(settings)
            .await
            .map_err(|error| {
                internal_error(format!("failed to restore thread settings: {error}"))
            })?;
        replacement
            .checkpoint_thread_settings()
            .await
            .map_err(|error| {
                internal_error(format!("failed to persist thread settings: {error}"))
            })?;
        Self::set_app_server_client_info(
            replacement.as_ref(),
            app_server_client_name,
            app_server_client_version,
        )
        .await?;
        self.ensure_listener_task_running(thread_id, replacement, thread_state)
            .await?;
        // Preserve the status channel the replacement listener just subscribed to.
        // Removing it stops that listener, and this runtime cannot start a second reader.
        self.thread_watch_manager
            .upsert_thread(&thread_id.to_string())
            .await;
        Ok(())
    }
}
