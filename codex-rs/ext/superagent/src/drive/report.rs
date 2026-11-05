//! What the engine reads from a driven turn and tells the Router: the worker's conclusion (its
//! last answer of the turn), the stop an anchored goal's audit calls for, and the notice text.

use super::Audit;
use super::DriveRecord;
use super::DriveState;
use super::UNCHANGED_LIMIT;
use crate::cards::truncate_chars;
use crate::host::TurnBrief;

/// How much of the conclusion a notice carries; the rest is read with `session/<id>`.
const NOTICE_CHARS: usize = 600;
const SUMMARY_CHARS: usize = 120;

/// The worker's conclusion: its last answer of the turn.
pub fn conclusion(turn: &TurnBrief) -> String {
    turn.assistant
        .iter()
        .rev()
        .map(|text| text.trim())
        .find(|text| !text.is_empty())
        .unwrap_or("（这一轮没有留下结论）")
        .to_owned()
}

fn summary(conclusion: &str) -> String {
    truncate_chars(
        &conclusion.split_whitespace().collect::<Vec<_>>().join(" "),
        SUMMARY_CHARS,
    )
}

/// A simple goal's turn ended: the conclusion waits for the Router.
pub fn reported(_record: &DriveRecord, conclusion: &str) -> DriveState {
    DriveState::Reported {
        summary: summary(conclusion),
    }
}

/// An anchored goal's turn ended: the audit decides whether the drive stops.
pub fn judged(record: &DriveRecord, conclusion: &str, audit: &Audit, unchanged: u32) -> DriveState {
    let findings = || {
        if audit.findings.is_empty() {
            audit.summary()
        } else {
            audit
                .findings
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join("；")
        }
    };
    if audit.deliverable() {
        DriveState::Deliverable {
            summary: audit.summary(),
        }
    } else if audit.status == "drift" {
        DriveState::NeedsDecision {
            reason: format!("偏移：{}", findings()),
        }
    } else if unchanged >= UNCHANGED_LIMIT {
        DriveState::NeedsDecision {
            reason: format!(
                "连续 {unchanged} 轮 Project 和锚定记录都没有变化：worker 没有回填（第 {} 步）",
                record.steps
            ),
        }
    } else if audit.status == "invalid" && audit.inconsistent.is_empty() {
        DriveState::NeedsDecision {
            reason: format!("判定无效：{}", findings()),
        }
    } else {
        DriveState::Reported {
            summary: summary(conclusion),
        }
    }
}

/// The notice the Router reads (and the person sees in the Router's window), or `None` when
/// there is nothing to tell (a step is out). A short header line, then the conclusion as the
/// worker wrote it, then where the original is and what to do next.
pub fn notice(record: &DriveRecord, state: &DriveState) -> Option<String> {
    let title = &record.title;
    let (head, next) = match state {
        DriveState::Driving => return None,
        DriveState::Reported { .. } => (
            format!("推进 · {title} · 第 {} 步结束", record.steps),
            "转告用户，或用 superagent_drive say 回它。",
        ),
        DriveState::Deliverable { summary } => (
            format!("推进 · {title} · 可交付，等用户验收（{summary}）"),
            "请用户验收；用户的反馈用 superagent_drive feedback 记入。",
        ),
        DriveState::NeedsDecision { reason } => (
            format!("推进 · {title} · 需要用户决定：{reason}"),
            "向用户说明原因，问一个具体问题。",
        ),
        DriveState::Paused { reason } => (
            format!("推进 · {title} · 已暂停：{reason}"),
            "告诉用户；用户要继续时用 superagent_drive resume。",
        ),
    };
    let mut lines = vec![head, format!("会话 {}", record.thread_id)];
    if let Some(report) = &record.report {
        lines.push("worker 的结论：".to_owned());
        lines.push(truncate_chars(report.text.trim(), NOTICE_CHARS));
        if let Some(audit) = &report.audit {
            lines.push(format!("判定：{audit}"));
        }
    }
    if !record.notes.is_empty() {
        lines.push(format!("用户在会话里说过：{}", record.notes.join(" / ")));
    }
    lines.push(format!("原文：superagent_read session/{}", record.thread_id));
    lines.push(format!("接下来：{next}"));
    Some(lines.join("\n"))
}
