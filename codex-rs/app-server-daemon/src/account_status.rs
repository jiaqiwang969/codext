//! Account-pool queries share the running daemon's credential managers.

use crate::client;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::RequestId;
use codex_uds::UnixStream;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tokio::time::Instant;
use tokio::time::timeout_at;
use tokio_tungstenite::WebSocketStream;

/// Read a profile's quota and usage through a dedicated daemon connection.
/// Selecting the profile affects only this connection, never an existing thread
/// or the default account. Credentials are loaded and refreshed by the daemon.
pub async fn query_account_status(
    socket_path: &Path,
    profile: &str,
    timeout: Duration,
) -> Result<Value> {
    let deadline = Instant::now() + timeout;
    let mut websocket = timeout_at(deadline, async {
        let mut websocket = client::connect(socket_path).await?;
        client::initialize(&mut websocket, /*experimental_api*/ true).await?;
        client::send_message(
            &mut websocket,
            &JSONRPCMessage::Notification(JSONRPCNotification {
                method: "initialized".to_string(),
                params: None,
            }),
        )
        .await?;
        Ok::<_, anyhow::Error>(websocket)
    })
    .await
    .context("timed out connecting to the background server")??;
    request(
        &mut websocket,
        deadline,
        /*id*/ 2,
        "account/profile/switch",
        Some(json!({ "name": profile })),
    )
    .await?;
    let limits = request(
        &mut websocket,
        deadline,
        /*id*/ 3,
        "account/rateLimits/read",
        /*params*/ None,
    )
    .await?;
    let (usage, usage_error) = match request(
        &mut websocket,
        deadline,
        /*id*/ 4,
        "account/usage/read",
        /*params*/ None,
    )
    .await
    {
        Ok(usage) => (usage, None),
        Err(error) => (json!({}), Some(format!("{error:#}"))),
    };
    Ok(json!({ "limits": limits, "usage": usage, "usage_error": usage_error }))
}

async fn request(
    websocket: &mut WebSocketStream<UnixStream>,
    deadline: Instant,
    id: i64,
    method: &str,
    params: Option<Value>,
) -> Result<Value> {
    let id = RequestId::Integer(id);
    timeout_at(deadline, async {
        client::send_message(
            websocket,
            &JSONRPCMessage::Request(JSONRPCRequest {
                id: id.clone(),
                method: method.to_string(),
                params,
                trace: None,
            }),
        )
        .await?;
        loop {
            match client::read_message(websocket).await? {
                JSONRPCMessage::Response(response) if response.id == id => {
                    return Ok(response.result);
                }
                JSONRPCMessage::Error(error) if error.id == id => {
                    bail!("{method}: {}", error.error.message);
                }
                JSONRPCMessage::Request(_)
                | JSONRPCMessage::Notification(_)
                | JSONRPCMessage::Response(_)
                | JSONRPCMessage::Error(_) => {}
            }
        }
    })
    .await
    .with_context(|| format!("{method}: background server timed out"))?
}
