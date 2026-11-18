//! The person's acceptance feedback on a driven goal: what they tried, which acceptance items
//! passed and which did not. It goes into the route anchor the way goal anchoring §1 and §3 ask:
//! the words verbatim as a `correction` request (with its source file) affecting those items,
//! the failed items reopened, and a route review after the last stage, since the person has now
//! decided how the route goes on. Only a plain anchor (no committed workspace) is written; a
//! committed workspace is the worker's to commit.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;
use serde_json::json;

use super::audit::workspace;

/// What the person reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feedback {
    /// The person's words, verbatim.
    pub text: String,
    /// Acceptance items the person checked and found met.
    pub passed: Vec<String>,
    /// Acceptance items the person found not met.
    pub failed: Vec<String>,
}

/// The next id of a kind (`U3`, `R2`) after the ones the list holds.
fn next_id(items: &[Value], prefix: char) -> String {
    let highest = items
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str))
        .filter_map(|id| id.strip_prefix(prefix))
        .filter_map(|n| n.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    format!("{prefix}{}", highest + 1)
}

/// Writes the feedback into the plain anchor of `project_home`; answers the request's id.
pub fn record(project_home: &Path, feedback: &Feedback) -> anyhow::Result<String> {
    anyhow::ensure!(!feedback.text.trim().is_empty(), "需要用户的原话（text）");
    anyhow::ensure!(
        !feedback.passed.is_empty() || !feedback.failed.is_empty(),
        "至少写明一项通过（passed）或没通过（failed）的验收项"
    );
    let found = workspace(project_home)?;
    anyhow::ensure!(
        !found.committed,
        "这个项目有工作区提交，验收反馈要由 worker 提交；请把原话直接告诉那个会话"
    );
    let path = project_home.join(&found.anchor);
    let mut anchor: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    let known: BTreeSet<String> = anchor
        .get("acceptance")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let named: BTreeSet<String> = feedback
        .passed
        .iter()
        .chain(&feedback.failed)
        .cloned()
        .collect();
    let unknown: Vec<&String> = named.iter().filter(|id| !known.contains(*id)).collect();
    anyhow::ensure!(
        unknown.is_empty(),
        "锚定记录里没有这些验收项：{}",
        unknown
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>()
            .join("、")
    );
    let both: Vec<&String> = feedback
        .passed
        .iter()
        .filter(|id| feedback.failed.contains(id))
        .collect();
    anyhow::ensure!(both.is_empty(), "同一项不能既通过又没通过");

    let requests = anchor["requests"].as_array().cloned().unwrap_or_default();
    let id = next_id(&requests, 'U');
    let source = format!("request-feedback-{}.txt", id.to_lowercase());
    std::fs::write(project_home.join(&source), &feedback.text)?;
    let mut requests = requests;
    requests.push(json!({
        "id": id,
        "kind": "correction",
        "text": feedback.text,
        "source": source,
        // Failed items first: the next step works on the first open item a correction names.
        "affects": feedback.failed.iter().chain(feedback.passed.iter()).collect::<Vec<_>>(),
    }));
    anchor["requests"] = Value::Array(requests);
    if let Some(items) = anchor["acceptance"].as_array_mut() {
        for item in items {
            let failed = item
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| feedback.failed.iter().any(|f| f == id));
            if failed {
                item["status"] = json!("open");
            }
        }
    }
    let last_stage = anchor["stages"]
        .as_array()
        .and_then(|stages| stages.last())
        .and_then(|stage| stage.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(after_stage) = last_stage {
        let mut reviews = anchor["reviews"].as_array().cloned().unwrap_or_default();
        let review = next_id(&reviews, 'R');
        reviews.push(json!({ "id": review, "after_stage": after_stage, "source": source }));
        anchor["reviews"] = Value::Array(reviews);
    }
    std::fs::write(&path, serde_json::to_string_pretty(&anchor)?)?;
    Ok(id)
}
