//! The judgment after each driven step: the engineering ontology's `project route-audit` on the
//! goal's Project workspace (`<project>/.engineering`). The workspace HEAD names the current
//! commit, and the commit names the Project record and the route anchor the audit reads.

use std::path::Path;
use std::path::PathBuf;

use serde_json::Value;

use crate::BoxFut;

/// The Project record and the route anchor of a workspace's current commit, relative to the
/// workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    pub project: String,
    pub anchor: String,
}

/// Reads the current commit of the workspace at `home`.
pub fn workspace(home: &Path) -> anyhow::Result<Workspace> {
    let read = |path: &Path| -> anyhow::Result<Value> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| anyhow::anyhow!("读不到 {}：{err}", path.display()))?;
        serde_json::from_str(&text)
            .map_err(|err| anyhow::anyhow!("{} 不是 JSON：{err}", path.display()))
    };
    let head = read(&home.join("HEAD.json"))?;
    let commit = head
        .pointer("/commit/path")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("{} 的 HEAD 没有指向提交", home.display()))?;
    let commit = read(&home.join(commit))?;
    let path_of = |name: &str| {
        commit
            .pointer(&format!("/proposal/{name}/path"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let project = path_of("project").ok_or_else(|| anyhow::anyhow!("当前提交没有 Project 记录"))?;
    let anchor =
        path_of("anchor").ok_or_else(|| anyhow::anyhow!("当前提交没有锚定记录（route-anchor）"))?;
    Ok(Workspace { project, anchor })
}

/// What `route-audit` answered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Audit {
    /// `anchored`, `drift` or `invalid`.
    pub status: String,
    pub verified: Vec<String>,
    pub open: Vec<String>,
    pub inconsistent: Vec<String>,
    /// One line per finding: its code, then whatever it names.
    pub findings: Vec<String>,
    pub project_sha256: String,
    pub anchor_sha256: String,
}

impl Audit {
    /// Reads an `oev2.route-audit/1` or `/2` report; a command failure is an error.
    pub fn parse(report: &Value) -> anyhow::Result<Self> {
        if let Some(error) = report.get("error") {
            anyhow::bail!(
                "route-audit 失败：{}",
                error
                    .get("reason")
                    .or_else(|| error.get("code"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            );
        }
        let status = report
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("route-audit 的结果没有 status"))?
            .to_owned();
        let ids = |name: &str| -> Vec<String> {
            report
                .get(name)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect()
        };
        let mut open = ids("delivery_open");
        for id in ids("open_acceptance") {
            if !open.contains(&id) {
                open.push(id);
            }
        }
        let findings = report
            .get("findings")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|finding| match finding {
                Value::Object(fields) => fields
                    .iter()
                    .map(|(key, value)| match value {
                        Value::String(text) => format!("{key}={text}"),
                        other => format!("{key}={other}"),
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
                other => other.to_string(),
            })
            .collect();
        let text = |name: &str| {
            report
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        Ok(Self {
            status,
            verified: ids("delivery_verified"),
            open,
            inconsistent: ids("delivery_inconsistent"),
            findings,
            project_sha256: text("project_sha256"),
            anchor_sha256: text("anchor_sha256"),
        })
    }

    /// Every acceptance item is verified, none is open or inconsistent.
    pub fn deliverable(&self) -> bool {
        self.status == "anchored"
            && self.open.is_empty()
            && self.inconsistent.is_empty()
            && !self.verified.is_empty()
    }

    /// One line for the ledger and the Router.
    pub fn summary(&self) -> String {
        let list = |ids: &[String]| {
            if ids.is_empty() {
                "无".to_owned()
            } else {
                ids.join("、")
            }
        };
        format!(
            "{}；已核实 {}；未闭合 {}；不一致 {}",
            self.status,
            list(&self.verified),
            list(&self.open),
            list(&self.inconsistent)
        )
    }
}

/// Judges a goal's Project; the engine calls it after each driven step.
///
/// Implementations read the workspace at `project_home` and must not change it: the worker owns
/// the Project records, the judge only reads them.
pub trait Judge: Send + Sync {
    fn audit<'a>(&'a self, project_home: &'a Path) -> BoxFut<'a, anyhow::Result<Audit>>;
}

/// The installed engineering ontology: `<root>/var/install/bin/oev2`.
pub struct Oev2Judge {
    pub root: PathBuf,
    pub bin: PathBuf,
}

impl Oev2Judge {
    /// `CODEXT_SUPERAGENT_OEV2_ROOT`, else the skill under `codex_home`.
    pub fn for_home(codex_home: &Path) -> Self {
        let root = std::env::var_os("CODEXT_SUPERAGENT_OEV2_ROOT")
            .filter(|root| !root.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| codex_home.join("skills/ontology-engineering-unified"));
        let bin = std::env::var_os("CODEXT_SUPERAGENT_OEV2_BIN")
            .filter(|bin| !bin.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("var/install/bin/oev2"));
        Self { root, bin }
    }
}

impl Judge for Oev2Judge {
    fn audit<'a>(&'a self, project_home: &'a Path) -> BoxFut<'a, anyhow::Result<Audit>> {
        Box::pin(async move {
            let workspace = workspace(project_home)?;
            let output = tokio::process::Command::new(&self.bin)
                .arg("--root")
                .arg(&self.root)
                .arg("--project-home")
                .arg(project_home)
                .args(["project", "route-audit", "--input"])
                .arg(&workspace.project)
                .arg("--anchor")
                .arg(&workspace.anchor)
                .current_dir(project_home)
                .stdin(std::process::Stdio::null())
                .output()
                .await
                .map_err(|err| anyhow::anyhow!("运行 {} 失败：{err}", self.bin.display()))?;
            // drift and invalid exit 1 with a report; a command failure exits 2 with one too.
            let report: Value = serde_json::from_slice(&output.stdout).map_err(|err| {
                anyhow::anyhow!(
                    "route-audit 没有给出 JSON（退出码 {:?}）：{err}；{}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr).trim()
                )
            })?;
            Audit::parse(&report)
        })
    }
}

/// The parts of the route anchor a drafted step names: the deliverable, every acceptance item
/// and the person's corrections.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Anchor {
    pub deliverable: String,
    /// (id, statement, status), in the anchor's order.
    pub acceptance: Vec<(String, String, String)>,
    /// (id, text, acceptance items it affects) of every `correction` request.
    pub corrections: Vec<(String, String, Vec<String>)>,
}

impl Anchor {
    /// Reads the anchor of the workspace's current commit.
    pub fn read(project_home: &Path) -> anyhow::Result<Self> {
        let workspace = workspace(project_home)?;
        let path = project_home.join(&workspace.anchor);
        let text = std::fs::read_to_string(&path)
            .map_err(|err| anyhow::anyhow!("读不到 {}：{err}", path.display()))?;
        Ok(Self::parse(&serde_json::from_str(&text)?))
    }

    pub fn parse(anchor: &Value) -> Self {
        let text = |value: &Value, name: &str| {
            value
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let list = |name: &str| {
            anchor
                .get(name)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        Self {
            deliverable: text(anchor, "deliverable"),
            acceptance: list("acceptance")
                .iter()
                .map(|item| {
                    (
                        text(item, "id"),
                        text(item, "statement"),
                        text(item, "status"),
                    )
                })
                .collect(),
            corrections: list("requests")
                .iter()
                .filter(|request| text(request, "kind") == "correction")
                .map(|request| {
                    let affects = request
                        .get("affects")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|id| id.as_str().map(str::to_owned))
                        .collect();
                    (text(request, "id"), text(request, "text"), affects)
                })
                .collect(),
        }
    }
}
