//! `thread/accountProfile/updated` reaches the other subscribers of a rebound thread.

use super::connection_handling_websocket::DEFAULT_READ_TIMEOUT;
use super::connection_handling_websocket::WsClient;
use super::connection_handling_websocket::connect_websocket;
use super::connection_handling_websocket::create_config_toml;
use super::connection_handling_websocket::read_jsonrpc_message;
use super::connection_handling_websocket::read_notification_for_method;
use super::connection_handling_websocket::read_response_for_id;
use super::connection_handling_websocket::send_initialize_request;
use super::connection_handling_websocket::send_request;
use super::connection_handling_websocket::spawn_websocket_server;
use anyhow::Context;
use anyhow::Result;
use app_test_support::create_fake_rollout_with_text_elements;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::to_response;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SwitchAuthProfileParams;
use codex_app_server_protocol::SwitchAuthProfileResponse;
use codex_app_server_protocol::ThreadAccountProfileUpdatedNotification;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use codex_login::login_with_api_key;
use pretty_assertions::assert_eq;
use std::path::Path;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::timeout;

const METHOD: &str = "thread/accountProfile/updated";
const PROFILE: &str = "next";

#[tokio::test]
async fn account_profile_switch_notifies_other_subscribers_only() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri(), "never")?;
    write_profile(codex_home.path())?;
    let thread_id = create_rollout(codex_home.path(), "2025-01-05T12-00-00")?;

    let (mut process, bind_addr) = spawn_websocket_server(codex_home.path()).await?;

    let result = async {
        let mut requester = connect_websocket(bind_addr).await?;
        let mut watcher = connect_websocket(bind_addr).await?;
        initialize_both_clients(&mut requester, &mut watcher).await?;

        send_request(
            &mut watcher,
            "thread/resume",
            /*id*/ 10,
            Some(serde_json::to_value(ThreadResumeParams {
                thread_id: thread_id.clone(),
                ..Default::default()
            })?),
        )
        .await?;
        let resume: ThreadResumeResponse =
            to_response(read_response_for_id(&mut watcher, /*id*/ 10).await?)?;
        assert_eq!(resume.thread.id, thread_id);

        let switch = switch_profile(&mut requester, /*id*/ 11, &thread_id).await?;
        assert_eq!(
            switch.response,
            SwitchAuthProfileResponse {
                name: PROFILE.to_string(),
                changed: true,
                thread_id: Some(thread_id.clone()),
            }
        );

        let notification = read_notification_for_method(&mut watcher, METHOD).await?;
        let notification: ThreadAccountProfileUpdatedNotification =
            serde_json::from_value(notification.params.context("notification params")?)?;
        assert_eq!(
            notification,
            ThreadAccountProfileUpdatedNotification {
                thread_id: thread_id.clone(),
                account_profile: PROFILE.to_string(),
            }
        );
        // The requester subscribed while its switch replaced the runtime; the response is its
        // acknowledgement.
        assert!(!switch.notified, "the requester must not be notified");
        assert_no_notification(&mut requester, Duration::from_millis(500)).await
    }
    .await;

    process
        .kill()
        .await
        .context("failed to stop websocket app-server process")?;
    result
}

#[tokio::test]
async fn account_profile_switch_of_unloaded_thread_notifies_no_one() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri(), "never")?;
    write_profile(codex_home.path())?;
    let thread_id = create_rollout(codex_home.path(), "2025-01-05T12-05-00")?;

    let (mut process, bind_addr) = spawn_websocket_server(codex_home.path()).await?;

    let result = async {
        let mut requester = connect_websocket(bind_addr).await?;
        let mut bystander = connect_websocket(bind_addr).await?;
        initialize_both_clients(&mut requester, &mut bystander).await?;

        let switch = switch_profile(&mut requester, /*id*/ 20, &thread_id).await?;
        assert_eq!(
            switch.response,
            SwitchAuthProfileResponse {
                name: PROFILE.to_string(),
                changed: true,
                thread_id: Some(thread_id.clone()),
            }
        );
        assert!(!switch.notified, "the requester must not be notified");
        // Nobody subscribes to an unloaded thread, and the notice is never broadcast.
        assert_no_notification(&mut bystander, Duration::from_millis(500)).await?;
        assert_no_notification(&mut requester, Duration::from_millis(250)).await
    }
    .await;

    process
        .kill()
        .await
        .context("failed to stop websocket app-server process")?;
    result
}

struct Switch {
    response: SwitchAuthProfileResponse,
    /// Whether the requester saw the notification before its response.
    notified: bool,
}

async fn switch_profile(stream: &mut WsClient, id: i64, thread_id: &str) -> Result<Switch> {
    send_request(
        stream,
        "account/profile/switch",
        id,
        Some(serde_json::to_value(SwitchAuthProfileParams {
            name: PROFILE.to_string(),
            thread_id: Some(thread_id.to_string()),
        })?),
    )
    .await?;
    let mut notified = false;
    loop {
        match read_jsonrpc_message(stream).await? {
            JSONRPCMessage::Response(response) if response.id == RequestId::Integer(id) => {
                return Ok(Switch {
                    response: to_response(response)?,
                    notified,
                });
            }
            JSONRPCMessage::Error(error) if error.id == RequestId::Integer(id) => {
                anyhow::bail!("account/profile/switch failed: {:?}", error.error);
            }
            JSONRPCMessage::Notification(notification) => {
                notified |= notification.method == METHOD;
            }
            JSONRPCMessage::Request(_) | JSONRPCMessage::Response(_) | JSONRPCMessage::Error(_) => {
            }
        }
    }
}

/// Reads until `wait_for` passes without a message, failing on a profile notice.
async fn assert_no_notification(stream: &mut WsClient, wait_for: Duration) -> Result<()> {
    while let Ok(message) = timeout(wait_for, read_jsonrpc_message(stream)).await {
        if let JSONRPCMessage::Notification(notification) = message?
            && notification.method == METHOD
        {
            anyhow::bail!("unexpected {METHOD}: {:?}", notification.params);
        }
    }
    Ok(())
}

async fn initialize_both_clients(first: &mut WsClient, second: &mut WsClient) -> Result<()> {
    send_initialize_request(first, /*id*/ 1, "ws_client_requester").await?;
    timeout(DEFAULT_READ_TIMEOUT, read_response_for_id(first, /*id*/ 1)).await??;

    send_initialize_request(second, /*id*/ 2, "ws_client_watcher").await?;
    timeout(DEFAULT_READ_TIMEOUT, read_response_for_id(second, /*id*/ 2)).await??;
    Ok(())
}

/// An account-pool profile with usable credentials under `CODEX_HOME/accounts`.
fn write_profile(codex_home: &Path) -> Result<()> {
    let profile_home = codex_home.join("accounts").join(PROFILE);
    std::fs::create_dir_all(&profile_home)?;
    login_with_api_key(
        &profile_home,
        "sk-next",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;
    Ok(())
}

fn create_rollout(codex_home: &Path, filename_ts: &str) -> Result<String> {
    create_fake_rollout_with_text_elements(
        codex_home,
        filename_ts,
        "2025-01-05T12:00:00Z",
        "Saved user message",
        Vec::new(),
        Some("mock_provider"),
        /*git_info*/ None,
    )
}
