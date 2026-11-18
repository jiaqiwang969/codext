//! `superagent/status`: the engine's status for the external watchdog (design §9).

use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::SuperagentStatusResponse;

use crate::error_code::internal_error;
use crate::message_processor::MessageProcessor;

impl MessageProcessor {
    pub(crate) async fn superagent_status(
        &self,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let service = self
            .superagent
            .as_ref()
            .ok_or_else(|| internal_error("superagent is off"))?;
        let status = codex_superagent_extension::status::status(service)
            .await
            .map_err(|error| internal_error(format!("superagent status failed: {error:#}")))?;
        let exit_code = status.exit_code;
        let status = serde_json::to_value(&status)
            .map_err(|error| internal_error(format!("superagent status is not JSON: {error}")))?;
        Ok(Some(SuperagentStatusResponse { exit_code, status }.into()))
    }
}
