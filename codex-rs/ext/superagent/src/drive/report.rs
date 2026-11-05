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

/// The notice the Router reads, or `None` when there is nothing to tell (a step is out).
pub fn notice(record: &DriveRecord, state: &DriveState) -> Option<String> {
    let head = format!("会话 {}（{}）", record.thread_id, record.title);
    let conclusion = record
        .report
        .as_ref()
        .map(|report| {
            let mut text = format!(
                "worker 的结论：{}",
                truncate_chars(&report.text, NOTICE_CHARS)
            );
            if let Some(audit) = &report.audit {
                text.push_str(&format!("\n判定：{audit}"));
            }
            text
        })
        .unwrap_or_default();
    let notes = if record.notes.is_empty() {
        String::new()
    } else {
        format!("\n用户在会话里说过：{}", record.notes.join(" / "))
    };
    let read = format!("原文：superagent_read session/{}", record.thread_id);
    let text = match state {
        DriveState::Driving => return None,
        DriveState::Reported { .. } => format!(
            "{head}第 {} 步结束。\n{conclusion}{notes}\n{read}\n转告用户，或用 superagent_drive say 回它。",
            record.steps
        ),
        DriveState::Deliverable { summary } => {
            format!("{head}可交付，等用户验收：{summary}\n{conclusion}\n{read}")
        }
        DriveState::NeedsDecision { reason } => {
            format!("{head}的推进需要用户决定：{reason}\n{conclusion}{notes}\n{read}")
        }
        DriveState::Paused { reason } => {
            format!("{head}的推进已暂停：{reason}\n{conclusion}{notes}")
        }
    };
    Some(text.trim_end().to_owned())
}
