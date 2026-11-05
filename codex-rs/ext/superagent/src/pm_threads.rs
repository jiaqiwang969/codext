//! The PM threads: the root threads the user talks to. Their ids live in
//! `$CODEX_HOME/superagent/pm-threads`, one per line, which the Mac launcher and `org-client`
//! write; the intake records their turns as Triggers and T-13 exposes the PM tools to them.
//!
//! A line may name the PM's project after the id, as `key=value` words: `project=<name>
//! repo=<path> base=<branch> worktrees=<path> account=<profile>`. One org then serves several
//! projects at once, one PM each: the executor builds a ticket's worktree from its PM's
//! project. Several PMs may share one project (one group each); `account=` pins a group
//! (the PM and its workers) to one account profile.

use std::path::Path;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct PmThreads {
    file: PathBuf,
}

/// The project a PM thread works on; every field is optional and falls back to the executor's
/// defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectConfig {
    pub name: Option<String>,
    pub repo: Option<PathBuf>,
    pub base_branch: Option<String>,
    pub worktree_root: Option<PathBuf>,
    /// The account profile the PM's workers run under (`account=<profile>`).
    pub account: Option<String>,
}

/// A line of the file: the thread id, then the project words.
fn parse_line(line: &str) -> Option<(String, ProjectConfig)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut words = line.split_whitespace();
    let id = words.next()?.to_owned();
    let mut project = ProjectConfig::default();
    for word in words {
        match word.split_once('=') {
            Some(("project", v)) => project.name = Some(v.to_owned()),
            Some(("repo", v)) => project.repo = Some(PathBuf::from(v)),
            Some(("base", v)) => project.base_branch = Some(v.to_owned()),
            Some(("worktrees", v)) => project.worktree_root = Some(PathBuf::from(v)),
            Some(("account", v)) => project.account = Some(v.to_owned()),
            _ => {}
        }
    }
    Some((id, project))
}

impl PmThreads {
    /// `dir` is the extension's directory under `CODEX_HOME` (`superagent/`).
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            file: dir.as_ref().join("pm-threads"),
        }
    }

    /// The list at an explicit path.
    pub fn from_file(file: impl AsRef<Path>) -> Self {
        Self {
            file: file.as_ref().to_path_buf(),
        }
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    fn entries(&self) -> Vec<(String, ProjectConfig)> {
        std::fs::read_to_string(&self.file)
            .map(|text| text.lines().filter_map(parse_line).collect())
            .unwrap_or_default()
    }

    /// The registered PM thread ids, in file order; empty when the file is missing.
    pub fn list(&self) -> Vec<String> {
        self.entries().into_iter().map(|(id, _)| id).collect()
    }

    /// The project a PM thread's line names, when it names any field.
    pub fn project_of(&self, thread_id: &str) -> Option<ProjectConfig> {
        self.entries()
            .into_iter()
            .find(|(id, _)| id == thread_id)
            .map(|(_, project)| project)
            .filter(|project| *project != ProjectConfig::default())
    }

    /// Every account a line pins a group to, in file order, without repeats.
    pub fn accounts(&self) -> Vec<String> {
        let mut accounts: Vec<String> = Vec::new();
        for (_, project) in self.entries() {
            if let Some(account) = project.account {
                if !accounts.contains(&account) {
                    accounts.push(account);
                }
            }
        }
        accounts
    }

    pub fn is_pm(&self, thread_id: &str) -> bool {
        self.list().iter().any(|id| id == thread_id)
    }

    /// Registers a thread as a PM; a no-op when it is one already.
    pub fn register(&self, thread_id: &str) -> std::io::Result<()> {
        if self.is_pm(thread_id) {
            return Ok(());
        }
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut text = std::fs::read_to_string(&self.file).unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(thread_id);
        text.push('\n');
        std::fs::write(&self.file, text)
    }
}

