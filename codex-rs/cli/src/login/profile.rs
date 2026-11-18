//! Stage named-profile logins until the account identity has been verified.
//! Failed or cancelled login attempts leave the current profile credentials intact.

use codex_core::config::Config;
use codex_login::AuthConfig;
use codex_login::AuthDotJson;
use codex_login::load_auth_dot_json;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use tempfile::TempDir;

struct AccountIdentity {
    account_id: String,
    user_id: Option<String>,
    email: Option<String>,
}

impl AccountIdentity {
    fn from_credentials(credentials: &AuthDotJson) -> Option<Self> {
        let tokens = credentials.tokens.as_ref()?;
        Some(Self {
            account_id: tokens
                .account_id
                .clone()
                .or_else(|| tokens.id_token.chatgpt_account_id.clone())?,
            user_id: tokens.id_token.chatgpt_user_id.clone(),
            email: tokens.id_token.email.clone(),
        })
    }

    fn matches(&self, actual: &Self) -> bool {
        self.account_id == actual.account_id
            && match (&self.user_id, &self.email) {
                (Some(user_id), _) => actual.user_id.as_ref() == Some(user_id),
                (None, Some(email)) => actual
                    .email
                    .as_ref()
                    .is_some_and(|actual_email| email.eq_ignore_ascii_case(actual_email)),
                (None, None) => true,
            }
    }
}

pub(super) struct ProfileLogin {
    target: AuthConfig,
    previous: Option<AuthDotJson>,
    expected: Option<AccountIdentity>,
    staging: TempDir,
    _login_lock: File,
}

impl ProfileLogin {
    fn prepare(config: &mut Config) -> io::Result<Self> {
        let target = config.auth_config();
        std::fs::create_dir_all(&target.codex_home)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let login_lock = options.open(target.codex_home.join(".login.lock"))?;
        login_lock.try_lock().map_err(|error| {
            io::Error::other(format!(
                "another login may already be updating this profile: {error}"
            ))
        })?;
        let previous = load_auth_dot_json(
            &target.codex_home,
            target.auth_credentials_store_mode,
            target.keyring_backend_kind,
        )?;
        // Inspect the file that will be replaced. Runtime auth loading can prefer
        // inherited access tokens, which must not decide a profile's identity.
        let expected = previous
            .as_ref()
            .and_then(AccountIdentity::from_credentials);
        let staging = tempfile::Builder::new()
            .prefix(".login-")
            .tempdir_in(&target.codex_home)?;
        config.codex_home = staging
            .path()
            .to_path_buf()
            .try_into()
            .map_err(io::Error::other)?;
        if let Some(expected) = expected.as_ref() {
            let account = expected.email.as_deref().unwrap_or(&expected.account_id);
            eprintln!("Sign in as {account} for this profile.");
        }
        Ok(Self {
            target,
            previous,
            expected,
            staging,
            _login_lock: login_lock,
        })
    }

    fn commit(self) -> io::Result<()> {
        let credentials = load_auth_dot_json(
            self.staging.path(),
            self.target.auth_credentials_store_mode,
            self.target.keyring_backend_kind,
        )?
        .ok_or_else(|| io::Error::other("login credentials are missing"))?;
        let identity = AccountIdentity::from_credentials(&credentials);
        if let Some(expected) = self.expected.as_ref()
            && !identity
                .as_ref()
                .is_some_and(|actual| expected.matches(actual))
        {
            let expected_account = expected.email.as_deref().unwrap_or(&expected.account_id);
            let actual_account = identity
                .as_ref()
                .map(|identity| identity.email.as_deref().unwrap_or(&identity.account_id))
                .unwrap_or("an account without a verifiable identity");
            return Err(io::Error::other(format!(
                "this profile belongs to {expected_account}, but you signed in as {actual_account}; existing credentials were kept. Sign in as the expected account or use a different profile name"
            )));
        }

        let current = load_auth_dot_json(
            &self.target.codex_home,
            self.target.auth_credentials_store_mode,
            self.target.keyring_backend_kind,
        )?;
        // A same-login token rotation is fine; a logout or a different completed
        // login during this browser flow must not be overwritten.
        let unchanged_login = match (&self.previous, &current) {
            (Some(previous), Some(current)) if previous.auth_mode == current.auth_mode => {
                match (&previous.tokens, &current.tokens) {
                    (Some(previous), Some(current)) => {
                        previous.login_owner_id == current.login_owner_id
                            && previous.account_id == current.account_id
                            && previous.id_token.chatgpt_user_id == current.id_token.chatgpt_user_id
                    }
                    _ => previous == current,
                }
            }
            (None, None) => true,
            _ => false,
        };
        if !unchanged_login {
            return Err(io::Error::other(
                "profile credentials changed during login; current credentials were kept. Retry login",
            ));
        }
        let mut output = tempfile::NamedTempFile::new_in(&self.target.codex_home)?;
        serde_json::to_writer_pretty(output.as_file_mut(), &credentials)?;
        output.write_all(b"\n")?;
        output.as_file().sync_all()?;
        output
            .persist(self.target.codex_home.join("auth.json"))
            .map_err(io::Error::other)?;
        let profile = self
            .target
            .codex_home
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        if let Some(identity) = identity {
            let account = identity.email.as_deref().unwrap_or(&identity.account_id);
            eprintln!("Profile {profile}: {account}");
        }
        eprintln!(
            "Credentials saved to {}",
            self.target.codex_home.join("auth.json").display()
        );
        Ok(())
    }
}

pub(super) fn prepare_or_exit(config: &mut Config) -> Option<ProfileLogin> {
    if std::env::var_os("CODEX_ACCOUNT_PROFILE").is_none_or(|profile| profile.is_empty()) {
        return None;
    }
    match ProfileLogin::prepare(config) {
        Ok(login) => Some(login),
        Err(error) => {
            eprintln!("Error preparing profile login: {error}");
            std::process::exit(1);
        }
    }
}

pub(super) fn finish_or_exit(login: Option<ProfileLogin>, message: &str) -> ! {
    if let Some(login) = login
        && let Err(error) = login.commit()
    {
        eprintln!("Error completing profile login: {error}");
        std::process::exit(1);
    }
    eprintln!("{message}");
    std::process::exit(0);
}
