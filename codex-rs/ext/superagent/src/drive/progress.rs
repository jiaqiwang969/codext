//! Whether a session's folder changed: the newest write under it. The audit of an anchored goal
//! counts a step without progress only when neither its records nor its folder changed
//! (`look.rs`), and the sweep tells a long step whose folder stands still (`sweep.rs`).
//!
//! The walk is bounded: build output, dependency and cache folders are skipped, and so is
//! everything past `MAX_ENTRIES` entries or `MAX_DEPTH` levels, so a large repository costs
//! milliseconds. A commit shows as the write of `.git/logs/HEAD`.

use std::path::Path;
use std::time::SystemTime;

/// Folders whose writes are not the worker's work, or are too many to look at.
const SKIPPED: &[&str] = &[
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".cache",
    "build",
    "dist",
    ".gradle",
    ".tox",
];
const MAX_ENTRIES: usize = 20_000;
const MAX_DEPTH: usize = 8;

/// The newest write under `folder`, as far as the bounded walk sees; `None` when it sees none.
pub(super) fn newest_write(folder: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    let mut seen = 0usize;
    let mut stack = vec![(folder.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > MAX_ENTRIES {
                return newest;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if meta.is_dir() {
                if name == ".git" {
                    // A commit, a checkout or a reset writes the log of HEAD.
                    let head = entry.path().join("logs").join("HEAD");
                    if let Ok(modified) = std::fs::metadata(head).and_then(|meta| meta.modified()) {
                        newest = Some(newest.map_or(modified, |at| at.max(modified)));
                    }
                } else if depth < MAX_DEPTH && !SKIPPED.contains(&name.as_ref()) {
                    stack.push((entry.path(), depth + 1));
                }
                continue;
            }
            if let Ok(modified) = meta.modified() {
                newest = Some(newest.map_or(modified, |at| at.max(modified)));
            }
        }
    }
    newest
}

/// Whether anything under `folder` was written after `since` (RFC 3339).
pub(super) fn changed_since(folder: &Path, since: &str) -> bool {
    let Ok(since) = chrono::DateTime::parse_from_rfc3339(since) else {
        return false;
    };
    let since: SystemTime = since.with_timezone(&chrono::Utc).into();
    newest_write(folder).is_some_and(|newest| newest > since)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn a_write_under_the_folder_counts_and_one_in_build_output_does_not() -> anyhow::Result<()> {
        let dir = tempfile::TempDir::new()?;
        std::fs::create_dir_all(dir.path().join("记录/S01"))?;
        std::fs::create_dir_all(dir.path().join("target/debug"))?;
        let before = chrono::Utc::now() - chrono::Duration::seconds(5);
        let since = before.to_rfc3339();
        assert_eq!(changed_since(dir.path(), &since), false);
        std::fs::write(dir.path().join("target/debug/out.bin"), "x")?;
        assert_eq!(
            changed_since(dir.path(), &since),
            false,
            "build output is skipped"
        );
        std::fs::write(dir.path().join("记录/S01/结论.md"), "做完了")?;
        assert_eq!(changed_since(dir.path(), &since), true);
        Ok(())
    }
}
