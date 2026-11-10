//! A move of the shown thread to another account profile realigns this window's connection.

use super::*;
use crate::app::agents_overview::tests::overview_thread;
use crate::app_server_session::AppServerSessionEvent;
use codex_app_server_client::AppServerEvent;
use codex_app_server_protocol::ThreadAccountProfileUpdatedNotification;
use codex_app_server_protocol::ThreadExtra;
use codex_app_server_protocol::ThreadStatus;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use codex_login::login_with_api_key;
use pretty_assertions::assert_eq;

fn moved(server: &AppServerSession, thread_id: ThreadId, profile: &str) -> AppServerSessionEvent {
    AppServerSessionEvent {
        backend_id: server.backend_id(),
        event: AppServerEvent::ServerNotification(Box::new(
            ServerNotification::ThreadAccountProfileUpdated(
                ThreadAccountProfileUpdatedNotification {
                    thread_id: thread_id.to_string(),
                    account_profile: profile.to_string(),
                },
            ),
        )),
    }
}

/// Dispatches every queued app event and returns the profile switches the server received.
async fn dispatch_and_take_switches(
    app: &mut App,
    tui: &mut crate::tui::Tui,
    server: &mut AppServerSession,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    requests: &session_lifecycle_requests::RecordedRequests,
) -> Result<Vec<serde_json::Value>> {
    while let Ok(event) = events.try_recv() {
        Box::pin(app.handle_event(tui, server, event)).await?;
    }
    Ok(
        std::mem::take(&mut *requests.lock().expect("request recorder lock"))
            .into_iter()
            .filter(|request| request.method == "account/profile/switch")
            .map(|request| request.params.unwrap_or_default())
            .collect(),
    )
}

#[tokio::test]
async fn moving_the_shown_thread_selects_its_profile_for_this_connection() -> Result<()> {
    let (mut app, mut events, _) = make_test_app_with_channels().await;
    let profile_home = app.config.codex_home.join("accounts").join("next");
    std::fs::create_dir_all(&profile_home)?;
    login_with_api_key(
        &profile_home,
        "sk-next",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;
    let (mut server, requests, proxy) =
        session_lifecycle_requests::start_recording_remote_app_server(&app.config).await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let shown = ThreadId::new();
    let other = ThreadId::new();
    app.primary_thread_id = Some(shown);
    app.active_thread_id = Some(shown);
    app.chat_widget
        .handle_thread_session(test_thread_session(shown, app.config.cwd.to_path_buf()));
    let mut other_thread = overview_thread(
        other,
        /*parent_thread_id*/ None,
        "Other task",
        ThreadStatus::Idle,
    );
    other_thread.extra = Some(ThreadExtra {
        account_profile: Some("previous".to_string()),
        account_id: Some("account-previous".to_string()),
    });
    app.agents_overview
        .threads
        .insert(other, Some(other_thread));
    while events.try_recv().is_ok() {}
    requests.lock().expect("request recorder lock").clear();

    // Another thread's move updates its overview row and leaves this connection alone.
    app.handle_backend_event(&server, moved(&server, other, "next"))
        .await;
    assert_eq!(
        app.agents_overview
            .threads
            .get(&other)
            .cloned()
            .flatten()
            .and_then(|thread| thread.extra),
        Some(ThreadExtra {
            account_profile: Some("next".to_string()),
            account_id: None,
        })
    );
    assert_eq!(
        dispatch_and_take_switches(&mut app, &mut tui, &mut server, &mut events, &requests).await?,
        Vec::<serde_json::Value>::new()
    );

    // The shown thread's move selects its profile for this connection, without a thread.
    app.handle_backend_event(&server, moved(&server, shown, "next"))
        .await;
    assert_eq!(
        dispatch_and_take_switches(&mut app, &mut tui, &mut server, &mut events, &requests).await?,
        vec![serde_json::json!({ "name": "next", "threadId": null })]
    );

    // Once this connection uses the profile, a repeated notice sends nothing.
    app.handle_backend_event(&server, moved(&server, shown, "next"))
        .await;
    assert_eq!(
        dispatch_and_take_switches(&mut app, &mut tui, &mut server, &mut events, &requests).await?,
        Vec::<serde_json::Value>::new()
    );

    server.shutdown().await?;
    proxy.await??;
    Ok(())
}
