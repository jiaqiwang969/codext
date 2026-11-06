//! Account-aware services follow the session lifetime, including cold replacement.

use super::*;
use codex_skills::SkillRootLoader;
use codex_utils_plugins::PluginSkillRoot;

/// Runtime services bound to one authentication profile.
///
/// A thread captures one of these runtimes when it starts. The runtime owns the
/// authentication manager and the account-aware model, plugin, and MCP managers,
/// while persistence and agent coordination remain owned by the shared thread
/// manager.
#[derive(Clone)]
pub struct ThreadRuntime {
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) models_manager: SharedModelsManager,
    pub(crate) plugins_manager: Arc<PluginsManager>,
    pub(crate) mcp_manager: Arc<McpManager>,
    pub(crate) auth_profile_id: Option<String>,
}

/// Authentication manager captured by a thread for extensions that need to
/// create account-bound backend clients.
#[derive(Clone)]
pub struct ThreadAuthManager(pub Arc<AuthManager>);

impl ThreadRuntime {
    pub fn auth_profile_id(&self) -> Option<&str> {
        self.auth_profile_id.as_deref()
    }

    pub(crate) fn from_session_services(services: &crate::state::SessionServices) -> Self {
        Self {
            auth_manager: Arc::clone(&services.auth_manager),
            models_manager: Arc::clone(&services.models_manager),
            plugins_manager: Arc::clone(&services.plugins_manager),
            mcp_manager: Arc::clone(&services.mcp_manager),
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
        config: &Config,
        auth_manager: Arc<AuthManager>,
        auth_profile_id: Option<String>,
    ) -> ThreadRuntime {
        self.state
            .runtime_for_auth(config, auth_manager, auth_profile_id)
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
        config: &Config,
        auth_manager: Arc<AuthManager>,
        auth_profile_id: Option<String>,
    ) -> ThreadRuntime {
        let skill_root_loader: Arc<dyn SkillRootLoader<PluginSkillRoot>> =
            self.skills_service.clone();
        let plugins_manager = Arc::new(PluginsManager::new_with_options(
            config.codex_home.to_path_buf(),
            self.session_source.restriction_product(),
            Arc::clone(&auth_manager),
            skill_root_loader,
        ));
        if let Some(analytics_events_client) = &self.analytics_events_client {
            plugins_manager.set_analytics_events_client(analytics_events_client.clone());
        }
        let mcp_manager = Arc::new(McpManager::new_with_extensions(
            Arc::clone(&plugins_manager),
            Arc::clone(&self.extensions),
            self.mcp_manager.codex_apps_tools_cache(),
        ));
        let models_manager = build_models_manager(config, Arc::clone(&auth_manager));
        ThreadRuntime {
            auth_manager,
            models_manager,
            plugins_manager,
            mcp_manager,
            auth_profile_id,
        }
    }

    pub(super) fn default_runtime_for_auth(&self, auth_manager: Arc<AuthManager>) -> ThreadRuntime {
        ThreadRuntime {
            auth_manager,
            models_manager: Arc::clone(&self.models_manager),
            plugins_manager: Arc::clone(&self.plugins_manager),
            mcp_manager: Arc::clone(&self.mcp_manager),
            auth_profile_id: None,
        }
    }

    pub(super) async fn account_runtime(&self, thread_id: ThreadId) -> Option<ThreadRuntime> {
        self.get_thread(thread_id)
            .await
            .ok()
            .map(|thread| ThreadRuntime::from_session_services(&thread.session.services))
    }
}
