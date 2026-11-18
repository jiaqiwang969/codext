//! Account-aware services follow the session lifetime, including cold replacement.

use super::*;

/// Runtime services bound to one authentication profile.
///
/// A thread captures one of these runtimes when it starts. The runtime owns the
/// authentication and runtime credential owners. Core creates model, plugin and
/// MCP services through its normal provider-aware startup path.
#[derive(Clone)]
pub struct ThreadRuntime {
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) runtime_auth: crate::runtime_auth::RuntimeAuthSlot,
    pub(crate) auth_profile_id: Option<String>,
}

/// Account API reads and threads share runtime registration only within one
/// credential owner. Keys identify the authentication manager, not the profile name.
#[derive(Default)]
pub(super) struct AccountRuntimeAuth(
    std::sync::Mutex<Vec<(std::sync::Weak<AuthManager>, crate::runtime_auth::RuntimeAuthSlot)>>,
);

impl AccountRuntimeAuth {
    fn for_auth(&self, auth: &Arc<AuthManager>) -> crate::runtime_auth::RuntimeAuthSlot {
        let mut owners = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        owners.retain(|(owner, _)| owner.strong_count() != 0);
        if let Some((_, slot)) = owners.iter().find(|(owner, _)| {
            owner.upgrade().is_some_and(|owner| Arc::ptr_eq(&owner, auth))
        }) {
            return Arc::clone(slot);
        }
        let slot = crate::runtime_auth::RuntimeAuthSlot::default();
        owners.push((Arc::downgrade(auth), Arc::clone(&slot)));
        slot
    }
}

impl ThreadRuntime {
    pub fn auth_profile_id(&self) -> Option<&str> {
        self.auth_profile_id.as_deref()
    }

    pub(crate) fn from_session_services(services: &crate::state::SessionServices) -> Self {
        Self {
            auth_manager: Arc::clone(&services.auth_manager),
            runtime_auth: Arc::clone(&services.runtime_auth),
            auth_profile_id: services
                .thread_extension_data
                .get::<ThreadRuntime>()
                .and_then(|runtime| runtime.auth_profile_id.clone()),
        }
    }
}

impl ThreadManager {
    /// Builds account-aware services for a thread without changing the shared coordinator.
    pub fn runtime_for_auth(
        &self,
        _config: &Config,
        auth_manager: Arc<AuthManager>,
        auth_profile_id: Option<String>,
    ) -> ThreadRuntime {
        self.state.runtime_for_auth(auth_manager, auth_profile_id)
    }

    /// Returns the authentication manager captured by a live thread.
    pub async fn auth_manager_for_thread(&self, thread_id: ThreadId) -> Option<Arc<AuthManager>> {
        self.get_thread(thread_id)
            .await
            .ok()
            .map(|thread| Arc::clone(&thread.session.services.auth_manager))
    }

    /// Returns the account-pool profile captured by a live thread runtime.
    pub async fn auth_profile_id_for_thread(&self, thread_id: ThreadId) -> Option<String> {
        self.state
            .account_runtime(thread_id)
            .await
            .and_then(|runtime| runtime.auth_profile_id)
    }
}

impl ThreadManagerState {
    pub(super) fn runtime_for_auth(
        &self,
        auth_manager: Arc<AuthManager>,
        auth_profile_id: Option<String>,
    ) -> ThreadRuntime {
        let runtime_auth = self.runtime_auth_slot_for_auth(&auth_manager);
        ThreadRuntime {
            auth_manager,
            runtime_auth,
            auth_profile_id,
        }
    }

    pub(super) fn default_runtime_for_auth(&self, auth_manager: Arc<AuthManager>) -> ThreadRuntime {
        self.runtime_for_auth(auth_manager, None)
    }

    pub(super) fn runtime_auth_slot_for_auth(
        &self,
        auth_manager: &Arc<AuthManager>,
    ) -> crate::runtime_auth::RuntimeAuthSlot {
        if Arc::ptr_eq(auth_manager, &self.auth_manager) {
            Arc::clone(&self.runtime_auth)
        } else {
            self.account_runtime_auth.for_auth(auth_manager)
        }
    }

    pub(super) async fn account_runtime(&self, thread_id: ThreadId) -> Option<ThreadRuntime> {
        self.get_thread(thread_id)
            .await
            .ok()
            .map(|thread| ThreadRuntime::from_session_services(&thread.session.services))
    }
}
