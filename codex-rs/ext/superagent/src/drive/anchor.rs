//! Anchors a goal the Router agreed with the person. When the session's project has no route
//! anchor yet, the engine writes the minimal records goal anchoring §8 reads, in the project's
//! `.engineering`: the person's request word for word (`request-root.txt`), the route anchor
//! (`route-anchor.json`, `oev2.route-anchor/2`) with the deliverable and the confirmed
//! acceptance items, and an empty Project record (`project.json`). It never overwrites an
//! existing anchor or workspace; the worker keeps both up to date from then on.

use std::path::Path;

use serde_json::json;

const ITEMS: usize = 12;
const TEXT_CHARS: usize = 600;

/// What the person confirmed through the Router.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Goal {
    /// The person's request, word for word.
    pub request: String,
    /// One sentence: what is delivered.
    pub deliverable: String,
    /// The checkable acceptance items, in order.
    pub acceptance: Vec<String>,
}

impl Goal {
    fn check(&self) -> anyhow::Result<()> {
        let too_long = |text: &str| text.chars().count() > TEXT_CHARS;
        anyhow::ensure!(!self.request.trim().is_empty(), "需要用户的原话（request）");
        anyhow::ensure!(
            !self.deliverable.trim().is_empty(),
            "需要一句话的交付物（deliverable）"
        );
        anyhow::ensure!(
            (1..=ITEMS).contains(&self.acceptance.len()),
            "验收清单需要 1 到 {ITEMS} 项"
        );
        anyhow::ensure!(
            self.acceptance.iter().all(|item| !item.trim().is_empty()),
            "验收项不能为空"
        );
        anyhow::ensure!(
            !too_long(&self.deliverable) && !self.acceptance.iter().any(|item| too_long(item)),
            "交付物和每个验收项不超过 {TEXT_CHARS} 字"
        );
        Ok(())
    }
}

/// The Project id: the project directory's name in lowercase ASCII words, else `project`.
fn project_id(project_home: &Path) -> String {
    let name = project_home
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_lowercase();
    let words: Vec<&str> = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    if words.is_empty() {
        "project".to_owned()
    } else {
        words.join("-")
    }
}

/// Writes the records into `project_home` (`<project>/.engineering`), creating it.
pub fn create(project_home: &Path, goal: &Goal) -> anyhow::Result<()> {
    goal.check()?;
    anyhow::ensure!(
        !project_home.join("HEAD.json").exists()
            && !project_home.join("route-anchor.json").exists(),
        "{} 已有锚定记录，不会重写",
        project_home.display()
    );
    std::fs::create_dir_all(project_home)?;
    let id = project_id(project_home);
    let items: Vec<String> = (1..=goal.acceptance.len())
        .map(|n| format!("A{n}"))
        .collect();
    let deliverable = goal.deliverable.trim();
    let anchor = json!({
        "schema": "oev2.route-anchor/2",
        "project_id": id,
        "root_object": "root",
        "deliverable": deliverable,
        "requests": [{
            "id": "U1",
            "kind": "root",
            "text": goal.request,
            "source": "request-root.txt",
            "affects": items,
        }],
        "functions": [{ "id": "F1", "statement": deliverable, "structures": ["root"] }],
        "acceptance": goal.acceptance.iter().zip(&items).map(|(statement, id)| json!({
            "id": id,
            "statement": statement.trim(),
            "functions": ["F1"],
            "status": "open",
            "decisions": [],
            "evidence": [],
        })).collect::<Vec<_>>(),
        "stages": [],
        "reviews": [],
        "policy": { "off_root_stage_limit": 2, "no_progress_stage_limit": 2, "review_interval": 5 },
    });
    let project = json!({
        "schema": "oev2.project/1",
        "project_id": id,
        "revision": "r1",
        "purpose": deliverable,
        "objects": { "root": "r1" },
        "conditions": { "scope": goal.request },
        "evidence": {},
        "decisions": [],
        "dependencies": {},
    });
    // The anchor last: a start that fails halfway leaves no anchor, so it can be retried.
    std::fs::write(project_home.join("request-root.txt"), &goal.request)?;
    std::fs::write(
        project_home.join("project.json"),
        serde_json::to_string_pretty(&project)?,
    )?;
    std::fs::write(
        project_home.join("route-anchor.json"),
        serde_json::to_string_pretty(&anchor)?,
    )?;
    Ok(())
}
