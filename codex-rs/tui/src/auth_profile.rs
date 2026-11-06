//! Account-pool profile discovery for the TUI.

use std::fs;
use std::io;
use std::path::Path;

/// List account-pool entries that contain a usable `auth.json`.
pub(crate) fn list_auth_profiles(codex_home: &Path) -> io::Result<Vec<String>> {
    let accounts_dir = codex_home.join("accounts");
    if !accounts_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut profiles = Vec::new();
    for entry in fs::read_dir(accounts_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if entry.path().join("auth.json").is_file() {
            profiles.push(name);
        }
    }
    profiles.sort();
    Ok(profiles)
}
