//! The drafter: writes a driven step's prompt from the goal's route anchor and the last audit,
//! in the stage format of the engineering ontology's goal anchoring (§2). It never writes a bare
//! "continue": each step names the acceptance item it must advance and how the worker reports
//! back.

use super::audit::Anchor;
use super::audit::Audit;
use crate::cards::truncate_chars;

/// How every driven step starts; the intake and the queue reconciliation recognise it.
pub const STEP_LABEL: &str = "[推进 ";

const TEXT_CHARS: usize = 300;
const CORRECTIONS: usize = 5;

/// What one step is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Focus {
    /// An item marked closed without enough support: correct the record first.
    Inconsistent(String),
    /// The open item to advance.
    Open(String),
}

/// The item a step works on: an inconsistent one first, then the open item a correction names
/// (the latest correction first), then the first open item in the anchor's order.
pub fn focus(anchor: &Anchor, audit: &Audit) -> Option<Focus> {
    if let Some(id) = audit.inconsistent.first() {
        return Some(Focus::Inconsistent(id.clone()));
    }
    let open = |id: &String| audit.open.contains(id);
    for (_, _, affects) in anchor.corrections.iter().rev() {
        if let Some(id) = affects.iter().find(|id| open(id)) {
            return Some(Focus::Open(id.clone()));
        }
    }
    anchor
        .acceptance
        .iter()
        .map(|(id, _, _)| id)
        .find(|id| open(id))
        .or_else(|| audit.open.first())
        .map(|id| Focus::Open(id.clone()))
}

/// The prompt of step `step` of the goal `title`. `notes` are what the person said in the
/// session since the last step.
pub fn compose(
    step: u32,
    title: &str,
    anchor: &Anchor,
    audit: &Audit,
    focus: &Focus,
    notes: &[String],
) -> String {
    let clip = |text: &str| truncate_chars(text.trim(), TEXT_CHARS);
    let statement = |id: &str| {
        anchor
            .acceptance
            .iter()
            .find(|(item, _, _)| item == id)
            .map(|(_, statement, status)| (clip(statement), status.clone()))
            .unwrap_or_else(|| ("（锚定记录里没有这一项）".to_owned(), "unknown".to_owned()))
    };
    let list = |ids: Vec<&String>| {
        if ids.is_empty() {
            "无".to_owned()
        } else {
            ids.iter()
                .map(|id| id.as_str())
                .collect::<Vec<_>>()
                .join("、")
        }
    };
    let id = match focus {
        Focus::Inconsistent(id) | Focus::Open(id) => id.as_str(),
    };
    let mut text = format!("{STEP_LABEL}第{step}步 · {}]\n", clip(title));
    if !anchor.deliverable.is_empty() {
        text.push_str(&format!("根交付物：{}\n", clip(&anchor.deliverable)));
    }
    let (what, status) = statement(id);
    match focus {
        Focus::Inconsistent(_) => text.push_str(&format!(
            "本步先纠正验收项 {id}：{what}。它标为 {status}，但判定认为依据不足（delivery_inconsistent）：改回未关闭，或补足它所依据的决定和证据。\n"
        )),
        Focus::Open(_) => {
            text.push_str(&format!("本步推进验收项 {id}：{what}（当前 {status}）。\n"))
        }
    }
    let others: Vec<&String> = audit.open.iter().filter(|other| *other != id).collect();
    text.push_str(&format!(
        "其余未闭合：{}；已核实：{}。\n",
        list(others),
        list(audit.verified.iter().collect())
    ));
    text.push_str(&format!("上一步判定（route-audit）：{}", audit.summary()));
    if !audit.findings.is_empty() {
        let findings: Vec<String> = audit.findings.iter().take(3).map(|f| clip(f)).collect();
        text.push_str(&format!("；发现：{}", findings.join("；")));
    }
    text.push('\n');
    if !notes.is_empty() {
        text.push_str("用户在会话里补充过的话（先确认已处理；属于纠正的，记入 route-anchor 的 requests，kind 为 correction，affects 写受影响的验收项）：\n");
        for note in notes.iter().rev().take(CORRECTIONS).rev() {
            text.push_str(&format!("- {}\n", clip(note)));
        }
    }
    text.push_str(&format!(
        "要求：
1. 开始前按工程本体 goal-anchoring §2 写一行阶段声明：本阶段对象与根对象的关系、推进哪项验收、预期得到什么。
2. 只做推进 {id} 的工作；不扩大范围，不改其他项目；需要推送、发布、删除或对外发消息时，先停下来问。
3. 结束前把本阶段回填到 route-anchor（stages、验收项 status、decisions、evidence）：有工作区提交的项目用 project workspace commit 提交，没有的直接更新 .engineering 里的 route-anchor.json 和 Project 记录。没有回填，这一步算没有推进。
4. 需要用户决定时，写出一个具体问题，然后结束本轮。"
    ));
    text
}
