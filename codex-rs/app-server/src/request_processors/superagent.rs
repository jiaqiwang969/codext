//! Internal organization requests reuse the client lifecycle and account checks.

use super::*;
use super::thread_processor::ThreadResumeTarget;
use codex_protocol::mcp::ClientMcpExtensions;

/// The connection the Superagent engine uses when it starts or resumes a thread: there is no client
/// behind it, so its responses are dropped by the transport and its subscription is removed again.
pub(crate) const SUPERAGENT_CONNECTION: ConnectionId = ConnectionId(u64::MAX - 1);

impl ThreadRequestProcessor {
    /// Starts a thread for the Superagent engine, as `thread/start` would for a client, and answers
    /// its id once it is up. The engine is not a client: the placeholder subscription the start
    /// registers is dropped, so the thread unloads like any other idle thread and the executor
    /// resumes it before delivering.
    pub(crate) async fn start_thread_internal(
        &self,
        params: ThreadStartParams,
        auth_profile_id: Option<String>,
    ) -> Result<ThreadId, JSONRPCErrorError> {
        let (started, thread_started) = tokio::sync::oneshot::channel();
        let request_id = ConnectionRequestId {
            connection_id: SUPERAGENT_CONNECTION,
            request_id: codex_app_server_protocol::RequestId::String(
                "superagent/thread/start".to_owned(),
            ),
        };
        let request_context = RequestContext::new(
            request_id.clone(),
            "thread/start",
            tracing::info_span!("superagent.thread_start"),
            None,
        );
        self.thread_start_inner(
            request_id,
            params,
            /*app_server_client_name*/ None,
            /*app_server_client_version*/ None,
            ClientMcpExtensions::default(),
            auth_profile_id,
            request_context,
            Some(started),
        )
        .await?;
        let thread_id = thread_started.await.map_err(|_| {
            internal_error("the worker thread did not start; see the app-server log for the thread/start error")
        })?;
        let _ = self
            .thread_unsubscribe_response_inner(
                ThreadUnsubscribeParams {
                    thread_id: thread_id.to_string(),
                },
                SUPERAGENT_CONNECTION,
            )
            .await;
        Ok(thread_id)
    }

    /// A request id on the engine's own connection, for requests the engine makes itself.
    pub(crate) fn superagent_request_id(label: &str) -> ConnectionRequestId {
        ConnectionRequestId {
            connection_id: SUPERAGENT_CONNECTION,
            request_id: codex_app_server_protocol::RequestId::String(format!(
                "superagent-{label}"
            )),
        }
    }

    /// A daemon-side resume with the caller's overrides (the superagent host applies a role
    /// file's model, effort and instructions to a cold worker this way).
    pub(crate) async fn resume_thread_with(
        &self,
        params: ThreadResumeParams,
        auth_profile_id: Option<String>,
    ) -> Result<(), JSONRPCErrorError> {
        self.thread_resume(
            ThreadResumeTarget::DaemonRecovery(None),
            params,
            /*app_server_client_name*/ None,
            /*app_server_client_version*/ None,
            ClientMcpExtensions::default(),
            auth_profile_id,
        )
        .await
        .map(|_| ())
    }
}

impl TurnRequestProcessor {
    /// A person's message steered into a running turn bypasses the turn-input contributors; the
    /// superagent engine records it here instead.
    pub(super) async fn note_steered_input(&self, thread_id: &str, turn_id: &str, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        if let Some(superagent) = &self.superagent {
            superagent.steered_input(thread_id, turn_id, text).await;
        }
    }
}

/// The text parts of a request's user input, as one message.
pub(super) fn v2_text(input: &[V2UserInput]) -> String {
    input
        .iter()
        .filter_map(|item| match item {
            V2UserInput::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
