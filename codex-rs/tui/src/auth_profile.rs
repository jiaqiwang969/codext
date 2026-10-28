//! Account-pool profile discovery and credential staging for the TUI.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::Path;

use tempfile::NamedTempFile;

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

/// Atomically replace the active `auth.json` with a named account-pool entry.
pub(crate) fn switch_auth_profile(codex_home: &Path, name: &str) -> io::Result<()> {
    if !is_safe_profile_name(name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "authentication profile name must be a single directory name",
        ));
    }

    let source = codex_home.join("accounts").join(name).join("auth.json");
    if !source.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("authentication profile `{name}` has no auth.json"),
        ));
    }

    let contents = fs::read(source)?;
    let temporary = NamedTempFile::new_in(codex_home)?;
    fs::write(temporary.path(), contents)?;
    temporary
        .persist(codex_home.join("auth.json"))
        .map_err(|error| error.error)?;
    Ok(())
}

fn is_safe_profile_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && Path::new(name).file_name() == Some(OsStr::new(name))
}
