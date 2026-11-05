//! The Router: the PM thread that oversees every session on the machine (command-center design
//! §7–§8). Its level is read from `$CODEX_HOME/superagent/router-mode` on each use:
//!
//! - `observe` (the default, also when the file is missing or unreadable): the Router sees every
//!   session and reports on them, but the engine sends nothing on its behalf. It gets only
//!   `superagent_read`, and its messages open no candidates.
//! - `drive`: the PM side as before: candidates, `superagent_route` and the other PM tools.
//!
//! In both levels the Router's context carries its rules and a bounded index of the most recently
//! active sessions as World State sections: a step is told only what changed since the step
//! before, and a compacted history gets the full text again.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::Weak;

use chrono::Local;
use chrono::TimeZone;
use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::WorldStateContributionInput;
use codex_extension_api::WorldStateSectionContribution;
use codex_extension_api::input_text;
use serde_json::Value;

use crate::SuperagentService;
use crate::cards::truncate_chars;
use crate::host::SessionBrief;

/// The file under the extension's directory that holds the Router's level.
pub const MODE_FILE: &str = "router-mode";

/// How many sessions the index shows: the most recently active ones.
pub const INDEX_ROWS: usize = 30;

/// How much of a session's name or first message an index row shows.
const TITLE_CHARS: usize = 48;

const RULES_ID: &str = "superagent_router";
const RULES_MARKERS: (&str, &str) = ("<superagent_router>", "</superagent_router>");
const INDEX_ID: &str = "superagent_index";
const INDEX_MARKERS: (&str, &str) = ("<superagent_index>", "</superagent_index>");
const INDEX_UPDATE_MARKERS: (&str, &str) =
    ("<superagent_index_update>", "</superagent_index_update>");

const OBSERVE_RULES: &str = "\
你是总 Router，当前处于“只观察”档。
- 你能看到本机所有会话：<superagent_index> 是按最近活动排序的索引，之后的 <superagent_index_update> 只写变化。细节用 superagent_read 读 `sessions`（翻页）或 `session/<id>`（最近几轮问答的摘要）。读取不会恢复或唤醒任何会话。
- 本档不向任何会话发送内容，不开工单，不改文件，不运行会改动系统的命令。
- 用户问进展时：先读相关会话，再按项目汇报结果、卡点和需要用户决定的事；引用会话 id；没读到的不要猜。
- 每条回复不超过 300 字。";

const DRIVE_RULES: &str = "\
你是总 Router，当前处于“推进”档：候选卡、superagent_route 和其他 PM 工具已开放，按 PM 规则路由和转达。<superagent_index> 仍是本机会话的只读索引，读细节用 superagent_read。";

/// The Router's level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouterMode {
    Observe,
    Drive,
}

impl RouterMode {
    /// Reads the level from `<dir>/router-mode`: only the word `drive` turns routing on.
    pub fn read(dir: &Path) -> Self {
        match std::fs::read_to_string(dir.join(MODE_FILE)) {
            Ok(text) if text.trim() == "drive" => Self::Drive,
            _ => Self::Observe,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Drive => "drive",
        }
    }

    fn rules(self) -> &'static str {
        match self {
            Self::Observe => OBSERVE_RULES,
            Self::Drive => DRIVE_RULES,
        }
    }
}

/// The context the Router thread gets each step: its rules and the session index.
pub struct RouterContext {
    service: Weak<SuperagentService>,
}

impl RouterContext {
    pub fn new(service: &Arc<SuperagentService>) -> Self {
        Self {
            service: Arc::downgrade(service),
        }
    }
}

impl ContextContributor for RouterContext {
    fn contribute_world_state<'a>(
        &'a self,
        input: WorldStateContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<WorldStateSectionContribution>> {
        Box::pin(async move {
            let Some(service) = self.service.upgrade() else {
                return Vec::new();
            };
            let pm_threads = service.pm_threads().list();
            if !pm_threads
                .iter()
                .any(|id| id == input.thread_store.level_id())
            {
                return Vec::new();
            }
            let mut sections = vec![rules_section(service.router_mode())];
            let Some(host) = service.host() else {
                return sections;
            };
            let limit = INDEX_ROWS + pm_threads.len();
            match host
                .list_sessions(
                    /*cursor*/ None,
                    u32::try_from(limit).unwrap_or(u32::MAX),
                )
                .await
            {
                Ok(page) => {
                    let home = std::env::var("HOME").unwrap_or_default();
                    let rows = page
                        .sessions
                        .iter()
                        .filter(|session| !pm_threads.contains(&session.id))
                        .take(INDEX_ROWS)
                        .map(|session| (session.id.clone(), index_row(session, &home)))
                        .collect();
                    sections.push(index_section(rows));
                }
                Err(err) => tracing::warn!("superagent: the Router index is unavailable: {err:#}"),
            }
            sections
        })
    }
}

fn starts_with_marker(role: &str, text: &str, markers: (&str, &str)) -> bool {
    role == "developer" && text.trim_start().starts_with(markers.0)
}

/// The Router's rules for its level; told again when the level changes or history lost them.
pub fn rules_section(mode: RouterMode) -> WorldStateSectionContribution {
    WorldStateSectionContribution::new(RULES_ID, move |previous| {
        let fragment = match previous {
            PreviousWorldStateSection::Known(Value::String(name)) if name == mode.name() => None,
            PreviousWorldStateSection::Known(_)
            | PreviousWorldStateSection::Unknown
            | PreviousWorldStateSection::Absent => Some(RenderedWorldStateFragment::new(
                "developer",
                RULES_MARKERS,
                input_text(mode.rules()),
            )),
        };
        (Some(Value::String(mode.name().to_owned())), fragment)
    })
    .with_retained_fragment_matcher(|role, text| starts_with_marker(role, text, RULES_MARKERS))
}

/// One index row: `id | title | directory | status | last activity | account`.
pub fn index_row(session: &SessionBrief, home: &str) -> String {
    let title = session
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(&session.preview);
    let title = truncate_chars(
        &title.split_whitespace().collect::<Vec<_>>().join(" "),
        TITLE_CHARS,
    );
    let cwd = match session.cwd.strip_prefix(home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => session.cwd.clone(),
    };
    let at = session
        .updated_at
        .and_then(|secs| Local.timestamp_opt(secs, 0).single())
        .map(|at| at.format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".to_owned());
    let account = session.account_profile.as_deref().unwrap_or("-");
    format!(
        "{} | {title} | {cwd} | {} | {at} | {account}",
        session.id, session.status
    )
}

/// The index section. `rows` are (id, row), most recently active first; the snapshot keeps them
/// by id, so a later step is told only the rows that changed and the ids that left the index.
pub fn index_section(rows: Vec<(String, String)>) -> WorldStateSectionContribution {
    WorldStateSectionContribution::new(INDEX_ID, move |previous| {
        let snapshot: serde_json::Map<String, Value> = rows
            .iter()
            .map(|(id, row)| (id.clone(), Value::String(row.clone())))
            .collect();
        let fragment = match previous {
            PreviousWorldStateSection::Known(Value::Object(before)) => index_update(&rows, before),
            PreviousWorldStateSection::Known(_)
            | PreviousWorldStateSection::Unknown
            | PreviousWorldStateSection::Absent => Some(full_index(&rows)),
        };
        (Some(Value::Object(snapshot)), fragment)
    })
    .with_retained_fragment_matcher(|role, text| starts_with_marker(role, text, INDEX_MARKERS))
}

fn full_index(rows: &[(String, String)]) -> RenderedWorldStateFragment {
    let mut text = format!(
        "本机会话，按最近活动排序，最多 {INDEX_ROWS} 个（不含 Router 自己）。列：id | 名称或首条消息 | 目录 | 状态 | 最近活动 | 账号\n"
    );
    if rows.is_empty() {
        text.push_str("（暂无会话）\n");
    }
    for (_, row) in rows {
        text.push_str(row);
        text.push('\n');
    }
    RenderedWorldStateFragment::new("developer", INDEX_MARKERS, input_text(text))
}

fn index_update(
    rows: &[(String, String)],
    before: &serde_json::Map<String, Value>,
) -> Option<RenderedWorldStateFragment> {
    let changed: Vec<&str> = rows
        .iter()
        .filter(|(id, row)| before.get(id).and_then(Value::as_str) != Some(row.as_str()))
        .map(|(_, row)| row.as_str())
        .collect();
    let current: BTreeSet<&str> = rows.iter().map(|(id, _)| id.as_str()).collect();
    let left: Vec<&str> = before
        .keys()
        .map(String::as_str)
        .filter(|id| !current.contains(id))
        .collect();
    if changed.is_empty() && left.is_empty() {
        return None;
    }
    let mut text = String::from("会话索引的变化（列同 <superagent_index>）：\n");
    for row in changed {
        text.push_str(row);
        text.push('\n');
    }
    if !left.is_empty() {
        text.push_str(&format!(
            "不在最近 {INDEX_ROWS} 个之列了：{}\n",
            left.join("、")
        ));
    }
    Some(RenderedWorldStateFragment::new(
        "developer",
        INDEX_UPDATE_MARKERS,
        input_text(text),
    ))
}
