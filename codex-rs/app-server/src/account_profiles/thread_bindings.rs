//! Durable account selections, separate from immutable conversation creator metadata.

use super::AccountProfileRegistry;
use super::is_safe_profile_name;
use codex_protocol::ThreadId;
use std::io::Write;

impl AccountProfileRegistry {
    pub(crate) fn thread_profile(&self, thread_id: ThreadId) -> Result<Option<String>, String> {
        let path = self
            .codex_home
            .join("thread-auth-profiles")
            .join(format!("{thread_id}.json"));
        let contents = match std::fs::read(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("failed to read thread account profile: {error}")),
        };
        let name: String = serde_json::from_slice(&contents)
            .map_err(|error| format!("invalid thread account profile: {error}"))?;
        if !is_safe_profile_name(&name) {
            return Err(format!(
                "invalid authentication profile for thread {thread_id}"
            ));
        }
        Ok(Some(name))
    }

    pub(crate) fn save_thread_profile(
        &self,
        thread_id: ThreadId,
        name: &str,
    ) -> Result<(), String> {
        let directory = self.codex_home.join("thread-auth-profiles");
        let save = || -> std::io::Result<()> {
            std::fs::create_dir_all(&directory)?;
            let mut file = tempfile::NamedTempFile::new_in(&directory)?;
            serde_json::to_writer(&mut file, name)?;
            file.flush()?;
            file.as_file().sync_all()?;
            file.persist(directory.join(format!("{thread_id}.json")))?;
            Ok(())
        };
        save().map_err(|error| format!("failed to save thread account profile: {error}"))
    }
}
