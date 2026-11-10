//! The account profile this connection reads account state and quota with.

use super::AppServerSession;
use color_eyre::eyre::Result;

impl AppServerSession {
    /// Selects `profile` as this connection's default without rebinding any thread, as bootstrap
    /// does after resume. Sends nothing when the connection already uses it, so following a thread
    /// does not reset account state that is already current.
    pub(crate) async fn align_auth_profile(&mut self, profile: String) -> Result<()> {
        if self.auth_profile.as_deref() == Some(profile.as_str()) {
            return Ok(());
        }
        self.switch_auth_profile(profile, /*thread_id*/ None)
            .await
            .map(|_| ())
    }
}
