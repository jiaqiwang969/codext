//! Keeps this window's account reads and quota polling on the account of the thread it shows.
//!
//! A connection reads account state and quota with its own default profile, which resume aligns
//! with the thread it attaches to. Another client or the Superagent engine can later move that
//! thread to another profile. The app-server then sends `thread/accountProfile/updated` to this
//! window, which selects the same profile for its connection without rebinding the thread. The
//! `AccountUpdated` notification that follows refreshes the header's account and restarts quota
//! polling for it.

use super::App;
use crate::app_event::AppEvent;
use crate::app_server_session::AppServerSession;
use codex_app_server_protocol::ThreadAccountProfileUpdatedNotification;
use codex_protocol::ThreadId;

impl App {
    pub(super) fn on_thread_account_profile_updated(
        &mut self,
        update: &ThreadAccountProfileUpdatedNotification,
    ) {
        if let Ok(thread_id) = ThreadId::from_string(&update.thread_id)
            && self.chat_widget.thread_id() == Some(thread_id)
        {
            self.app_event_tx.send(AppEvent::AlignAccountProfile {
                thread_id,
                profile: update.account_profile.clone(),
            });
        }
    }

    pub(super) async fn align_account_profile(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        profile: String,
    ) {
        // The window may show another thread by now.
        if self.chat_widget.thread_id() != Some(thread_id) {
            return;
        }
        if let Err(error) = app_server.align_auth_profile(profile.clone()).await {
            self.chat_widget.add_error_message(format!(
                "This conversation moved to account profile `{profile}`, but this window could not follow it: {error:#}"
            ));
        }
    }
}
