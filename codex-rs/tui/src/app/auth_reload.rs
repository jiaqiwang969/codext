//! Profile file reload and account state recovery for the TUI.

use super::*;
use crate::app_server_session::account_state_from_reload_account_response;
use crate::status::StatusAccountDisplay;
use crate::status::plan_type_display_name;
use codex_protocol::account::PlanType;

const AUTH_RELOAD_RETRY_DELAY: Duration = Duration::from_secs(5);
const AUTH_RELOAD_MAX_ATTEMPTS: u8 = 3;
/// Upper bound for retries when a shared daemon reports the reload was skipped
/// because another client's turn is running. If the daemon is still busy after
/// this many retries the pending switch is still picked up by the
/// request-boundary auth reload on the next turn start.
const AUTH_RELOAD_BUSY_MAX_ATTEMPTS: u8 = 24;

/// Refresh-relevant account identity used to detect and describe auth changes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthIdentity {
    email: Option<String>,
    plan_type: Option<PlanType>,
    has_chatgpt_account: bool,
}

impl AuthIdentity {
    fn from_chat_widget(chat_widget: &ChatWidget) -> Self {
        Self::from_parts(
            &chat_widget.status_account_display().cloned(),
            chat_widget.current_plan_type(),
            chat_widget.has_chatgpt_account(),
        )
    }

    fn from_parts(
        status_account_display: &Option<StatusAccountDisplay>,
        plan_type: Option<PlanType>,
        has_chatgpt_account: bool,
    ) -> Self {
        let email = match status_account_display {
            Some(StatusAccountDisplay::ChatGpt { email, .. }) => email.clone(),
            Some(StatusAccountDisplay::ApiKey) | None => None,
        };
        Self {
            email,
            plan_type,
            has_chatgpt_account,
        }
    }

    fn display_label(&self) -> String {
        if !self.has_chatgpt_account {
            return "API key".to_string();
        }
        let email = self.email.as_deref().unwrap_or("unknown email");
        let plan = self
            .plan_type
            .map(plan_type_display_name)
            .unwrap_or_else(|| "unknown plan".to_string());
        format!("{email}({plan})")
    }
}

fn auth_change_message(previous: &AuthIdentity, next: &AuthIdentity) -> String {
    format!(
        "Account changed from {} to {}.",
        previous.display_label(),
        next.display_label()
    )
}

impl App {
    fn schedule_auth_reload_retry(&self, attempt: u8) {
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(AUTH_RELOAD_RETRY_DELAY).await;
            app_event_tx.send(AppEvent::AuthFileChangedRetry { attempt });
        });
    }

    /// Reload auth from storage after `auth.json` changed on disk.
    ///
    /// Reload is deferred while a task is running so auth is never hot-swapped in
    /// the middle of a turn. Transient storage failures retry with a short
    /// backoff before surfacing a warning, and the previously cached auth is kept
    /// until a reload actually succeeds.
    pub(crate) async fn handle_auth_file_changed(
        &mut self,
        app_server: &mut AppServerSession,
        attempt: u8,
    ) {
        if !self.config.features.enabled(Feature::CodextAuthReload) {
            return;
        }
        if self.chat_widget.is_task_running() {
            self.chat_widget.defer_auth_reload_until_idle(attempt);
            return;
        }
        let previous = AuthIdentity::from_chat_widget(&self.chat_widget);
        match app_server.reload_account_from_storage().await {
            Ok(account) => {
                if account.auth_reload_skipped {
                    // The shared daemon skipped the reload because a turn is
                    // running in another client; keep retrying while it stays
                    // busy instead of dropping the account switch.
                    if attempt < AUTH_RELOAD_BUSY_MAX_ATTEMPTS {
                        self.schedule_auth_reload_retry(attempt.saturating_add(1));
                    } else {
                        self.chat_widget
                            .on_auth_reload_completed(/*identity_changed*/ false);
                    }
                    return;
                }
                let (display, plan_type, has_chatgpt_account, has_codex_backend_auth) =
                    account_state_from_reload_account_response(&account);
                let next = AuthIdentity::from_parts(&display, plan_type, has_chatgpt_account);
                let changed = account.auth_changed;
                self.chat_widget.update_account_state(
                    display,
                    plan_type,
                    has_chatgpt_account,
                    has_codex_backend_auth,
                );
                if changed {
                    self.chat_widget.handle_auth_identity_changed();
                    self.chat_widget
                        .add_to_history(history_cell::new_warning_event(auth_change_message(
                            &previous, &next,
                        )));
                }
                self.chat_widget.on_auth_reload_completed(changed);
                if has_chatgpt_account {
                    let reset_hint_request_id =
                        self.chat_widget.start_rate_limit_reset_startup_check();
                    self.refresh_rate_limits(
                        app_server,
                        RateLimitRefreshOrigin::StartupPrefetch {
                            reset_hint_request_id,
                        },
                    );
                } else {
                    // API-key auth has no ChatGPT usage limits to poll.
                    self.chat_widget.on_rate_limit_snapshot(/*snapshot*/ None);
                }
            }
            Err(_err) if attempt < AUTH_RELOAD_MAX_ATTEMPTS => {
                self.schedule_auth_reload_retry(attempt.saturating_add(1));
            }
            Err(err) => {
                tracing::warn!(%err, "failed to reload auth from storage");
                self.chat_widget
                    .on_auth_reload_completed(/*identity_changed*/ false);
                self.chat_widget
                    .add_to_history(history_cell::new_warning_event(
                        "Failed to reload auth after auth.json changed.".to_string(),
                    ));
            }
        }
    }
}
