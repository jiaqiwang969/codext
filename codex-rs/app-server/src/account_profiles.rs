//! Connection-scoped account profiles for the shared app-server daemon.
//!
//! A profile owns its credential reader and account-aware runtime services. The
//! daemon keeps the profile managers alive independently, so selecting one
//! profile never rewrites the default `auth.json` or changes a running thread.

use codex_core::ThreadManager;
use codex_core::ThreadRuntime;
use codex_core::config::Config;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::AuthConfig;
use codex_login::WorkspaceRouting;
use codex_login::WorkspaceRoutingRequest;
use codex_login::WorkspaceRoutingResolver;
use std::collections::HashMap;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::RwLock as StdRwLock;
use tokio::sync::Mutex;
use tokio::sync::RwLock;

use crate::request_processors::account_processor::AccountRequestProcessor;

#[derive(Clone)]
pub(crate) struct AccountProfileRegistry {
    codex_home: PathBuf,
    auth_config: AuthConfig,
    default_auth_manager: Arc<StdRwLock<Option<Arc<AuthManager>>>>,
    profiles: Arc<RwLock<HashMap<String, Arc<AuthManager>>>>,
    routing_resolvers: Arc<RwLock<HashMap<String, Arc<dyn WorkspaceRoutingResolver>>>>,
    routing_owner: Arc<StdRwLock<Weak<AccountRequestProcessor>>>,
    load_lock: Arc<Mutex<()>>,
}

impl AccountProfileRegistry {
    pub(crate) fn new(config: &Config) -> Self {
        Self {
            codex_home: config.codex_home.to_path_buf(),
            auth_config: config.auth_config(),
            default_auth_manager: Arc::new(StdRwLock::new(None)),
            profiles: Arc::new(RwLock::new(HashMap::new())),
            routing_resolvers: Arc::new(RwLock::new(HashMap::new())),
            routing_owner: Arc::new(StdRwLock::new(Weak::new())),
            load_lock: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn set_routing_owner(&self, owner: Weak<AccountRequestProcessor>) {
        *self.routing_owner.write().expect("routing owner lock poisoned") = owner;
    }

    pub(crate) fn set_default_auth_manager(&self, auth_manager: Arc<AuthManager>) {
        *self
            .default_auth_manager
            .write()
            .expect("default auth manager lock poisoned") = Some(auth_manager);
    }

    pub(crate) async fn auth_manager_for_profile(
        &self,
        name: &str,
    ) -> Result<Arc<AuthManager>, String> {
        let auth_manager = self.profile_auth_manager(name).await?;
        self.install_routing_resolver(name, &auth_manager).await;
        if auth_manager.auth_cached().is_none() {
            return Err(format!(
                "authentication profile `{name}` has no usable credentials"
            ));
        }
        Ok(auth_manager)
    }

    pub(crate) async fn runtime_for_profile(
        &self,
        thread_manager: &ThreadManager,
        config: &Config,
        profile_id: Option<&str>,
    ) -> Result<Option<ThreadRuntime>, String> {
        let Some(profile_id) = profile_id else {
            return Ok(None);
        };
        let auth_manager = self.auth_manager_for_profile(profile_id).await?;
        Ok(Some(thread_manager.runtime_for_auth(
            config,
            auth_manager,
            Some(profile_id.to_owned()),
        )))
    }

    pub(crate) async fn runtime_for_account_id(
        &self,
        thread_manager: &ThreadManager,
        config: &Config,
        account_id: Option<&str>,
        fallback_profile_id: Option<&str>,
    ) -> Result<Option<ThreadRuntime>, String> {
        let profile_id = if let Some(account_id) =
            account_id.filter(|account_id| !account_id.is_empty())
        {
            if let Some(profile_id) = self.profile_for_account_id(account_id).await {
                Some(profile_id)
            } else if self.default_auth_matches(account_id) {
                return Ok(None);
            } else {
                return Err(format!(
                    "no authentication profile matches thread account `{account_id}`"
                ));
            }
        } else {
            fallback_profile_id.map(str::to_owned)
        };
        self.runtime_for_profile(
            thread_manager,
            config,
            profile_id.as_deref(),
        )
        .await
    }

    fn default_auth_matches(&self, account_id: &str) -> bool {
        self.default_auth_manager
            .read()
            .expect("default auth manager lock poisoned")
            .as_ref()
            .and_then(|auth_manager| auth_manager.auth_cached())
            .as_ref()
            .and_then(CodexAuth::get_account_id)
            .as_deref()
            == Some(account_id)
    }

    pub(crate) async fn profile_for_account_id(&self, account_id: &str) -> Option<String> {
        let entries = std::fs::read_dir(self.codex_home.join("accounts")).ok()?;
        let mut profile_names = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let file_type = entry.file_type().ok()?;
                file_type.is_dir().then(|| entry.file_name().to_string_lossy().into_owned())
            })
            .filter(|name| is_safe_profile_name(name))
            .collect::<Vec<_>>();
        profile_names.sort();

        for profile_name in profile_names {
            let Ok(auth_manager) = self.auth_manager_for_profile(&profile_name).await else {
                continue;
            };
            if auth_manager
                .auth_cached()
                .as_ref()
                .and_then(CodexAuth::get_account_id)
                .as_deref()
                == Some(account_id)
            {
                return Some(profile_name);
            }
        }
        None
    }

    async fn profile_auth_manager(&self, name: &str) -> Result<Arc<AuthManager>, String> {
        if !is_safe_profile_name(name) {
            return Err(format!(
                "authentication profile `{name}` must be a single directory name"
            ));
        }
        if let Some(auth_manager) = self.profiles.read().await.get(name).cloned() {
            return Ok(auth_manager);
        }

        let _load_guard = self.load_lock.lock().await;
        if let Some(auth_manager) = self.profiles.read().await.get(name).cloned() {
            return Ok(auth_manager);
        }

        let profile_home = self.codex_home.join("accounts").join(name);
        if !profile_home.is_dir() {
            return Err(format!(
                "authentication profile `{name}` does not exist under `{}`",
                self.codex_home.join("accounts").display()
            ));
        }
        let mut auth_config = self.auth_config.clone();
        auth_config.codex_home = profile_home;
        // Account-pool entries are explicit per-profile auth.json files. Do not
        // resolve them through the daemon's default keyring namespace.
        auth_config.auth_credentials_store_mode = AuthCredentialsStoreMode::File;
        let auth_manager = AuthManager::shared_from_auth_config(auth_config, false)
            .await
            .map_err(|error| format!("failed to load authentication profile `{name}`: {error}"))?;
        if auth_manager.auth_cached().is_none() {
            return Err(format!(
                "authentication profile `{name}` has no usable credentials"
            ));
        }
        self.profiles
            .write()
            .await
            .insert(name.to_owned(), Arc::clone(&auth_manager));
        Ok(auth_manager)
    }

    async fn install_routing_resolver(&self, name: &str, auth_manager: &Arc<AuthManager>) {
        let mut routing_resolvers = self.routing_resolvers.write().await;
        if routing_resolvers.contains_key(name) {
            return;
        }
        let owner = self
            .routing_owner
            .read()
            .expect("routing owner lock poisoned")
            .clone();
        if owner.upgrade().is_none() {
            return;
        }
        let resolver: Arc<dyn WorkspaceRoutingResolver> = Arc::new(ProfileWorkspaceRoutingResolver {
            profile_id: name.to_owned(),
            owner,
        });
        auth_manager.set_workspace_routing_resolver(Arc::downgrade(&resolver));
        routing_resolvers.insert(name.to_owned(), resolver);
    }
}

struct ProfileWorkspaceRoutingResolver {
    profile_id: String,
    owner: Weak<AccountRequestProcessor>,
}

impl WorkspaceRoutingResolver for ProfileWorkspaceRoutingResolver {
    fn resolve(
        &self,
        request: WorkspaceRoutingRequest,
    ) -> Pin<Box<dyn Future<Output = std::io::Result<Option<WorkspaceRouting>>> + Send + '_>> {
        let profile_id = self.profile_id.clone();
        let Some(owner) = self.owner.upgrade() else {
            return Box::pin(async {
                Err(std::io::Error::other(
                    "workspace routing owner is unavailable",
                ))
            });
        };
        Box::pin(async move {
            owner
                .resolve_profile_workspace_routing(&profile_id, request)
                .await
        })
    }
}

pub(crate) fn is_safe_profile_name(name: &str) -> bool {
    !name.is_empty()
        && Path::new(name)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        && Path::new(name).components().count() == 1
}
