//! The Router: the PM thread that oversees every session on the machine (command-center design
//! §7–§8). Its level is read from `$CODEX_HOME/superagent/router-mode` on each use:
//!
//! - `observe` (the default, also when the file is missing or unreadable): the Router sees every
//!   session and reports on them, but the engine sends nothing on its behalf. It gets only
//!   `superagent_read`, and its messages open no candidates.
//! - `drive`: the Router drives sessions toward anchored goals with `superagent_drive`; its own
//!   messages still open no candidates, so it never routes a request into a ticket by mistake.
//! - `org`: the organization as before (candidates, `superagent_route`, tickets and workers), with
//!   `superagent_drive` beside it.
//!
//! In every level the Router's context carries, as World State sections, its rules, a bounded
//! index of the most recently active sessions and the sessions that wait for the person (an
//! error, an approval, an answer): a step is told only what changed since the step before, and a
//! compacted history gets the full text again.

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
use sha2::Digest;
use sha2::Sha256;

mod drives;

pub use drives::drive_attention;
pub use drives::drives_section;

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
const ATTENTION_ID: &str = "superagent_attention";
const ATTENTION_MARKERS: (&str, &str) = ("<superagent_attention>", "</superagent_attention>");
const ATTENTION_UPDATE_MARKERS: (&str, &str) = (
    "<superagent_attention_update>",
    "</superagent_attention_update>",
);

const OBSERVE_RULES: &str = "\
你是总 Router，当前处于“只观察”档。
- 上下文里已经有两份清单，不用读：<superagent_index> 是本机会话按最近活动排序的索引（之后的 <superagent_index_update> 只写变化）；<superagent_attention> 是需要用户处理的会话（出错、等批准、等回答），每个问题只出一次，解决后另有说明。
- 要细节时用 superagent_read 读 `sessions`（翻页）或 `session/<id>`（最近几轮问答的摘要）。读取不会恢复或唤醒任何会话。
- 本档不向任何会话发送内容，不开工单，不改文件，不运行会改动系统的命令。
- 用户问进展时：先读相关会话，再按项目汇报结果、卡点和需要用户决定的事；引用会话 id；没读到的不要猜。用户问“有什么要我处理的”时先看 <superagent_attention>。
- 每条回复不超过 300 字。";

const DRIVE_RULES: &str = "\
你是总 Router，当前处于“推进”档。
- 上下文里的 <superagent_index>（会话索引）、<superagent_attention>（等用户处理）和 <superagent_drives>（由引擎推进的会话）已经是最新清单，不用读。细节用 superagent_read 读 `sessions` 或 `session/<id>`。
- 用户要你推进某个会话时：确认它的项目有 .engineering 且已提交 route-anchor（验收清单），再用 superagent_drive start（thread_id、project、title）。之后引擎在每个回合结束时运行 route-audit 并发出下一步，你不用写“继续”。
- 项目还没有锚定记录时：和用户一起定下根请求原话、一句话交付物、逐条可核对的验收清单，请用户明确确认；确认后 start 时带上 request（原话照抄）、deliverable、acceptance，引擎先建立锚定再推进。不要替用户编验收项，用户没确认就不要 start。
- 用户说自己来：superagent_drive pause；说交回：resume（可带 max_steps 放宽步数）；不再推进：stop；问发过什么：history。
- 用户试用或检查后说哪里行、哪里不行：用 superagent_drive feedback，text 照抄原话，passed 写用户认可的验收项编号，failed 写不行的编号（可带 max_steps）。拿不准对应哪一项时先问用户。引擎会记入锚定记录并继续推进。
- 可交付、需要决定、已暂停会出现在 <superagent_attention>，引擎也会送来 {\"kind\":\"notice\"…} 通知（不是用户在说话）：用一句话转告用户。可交付时请用户验收；需要决定时说明原因，问一个具体问题。最终验收由用户做。
- 本档不开工单、不新建会话，也不直接向会话发内容：推进只经 superagent_drive。每条回复不超过 300 字。";

const ORG_RULES: &str = "\
你是总 Router，当前处于“组织”档：除了 superagent_drive 推进已有会话（规则同推进档），还可以用候选卡和 superagent_route 开工单，由引擎新建 worker 完成。
- 上下文里的 <superagent_index>、<superagent_attention>、<superagent_drives> 已经是最新清单，不用读。
- 用户要推进某个已有会话时，用 superagent_drive；用户提出新的工作时，按候选卡用 superagent_route。
- 引擎送来的通知、转达卡不是用户在说话：用一句话转告结论和需要用户做的事。每条回复不超过 300 字。";

/// The Router's level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouterMode {
    Observe,
    Drive,
    Org,
}

impl RouterMode {
    /// Reads the level from `<dir>/router-mode`: `drive` or `org`; anything else observes.
    pub fn read(dir: &Path) -> Self {
        match std::fs::read_to_string(dir.join(MODE_FILE))
            .as_deref()
            .map(str::trim)
        {
            Ok("drive") => Self::Drive,
            Ok("org") => Self::Org,
            Ok(_) | Err(_) => Self::Observe,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Drive => "drive",
            Self::Org => "org",
        }
    }

    /// Whether the Router's own messages become candidates for tickets.
    pub fn routes_requests(self) -> bool {
        match self {
            Self::Org => true,
            Self::Observe | Self::Drive => false,
        }
    }

    fn rules(self) -> &'static str {
        match self {
            Self::Observe => OBSERVE_RULES,
            Self::Drive => DRIVE_RULES,
            Self::Org => ORG_RULES,
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
                    let rows: Vec<(String, String, String)> = page
                        .sessions
                        .iter()
                        .filter(|session| !pm_threads.contains(&session.id))
                        .take(INDEX_ROWS)
                        .map(|session| {
                            (
                                session.id.clone(),
                                session.status.clone(),
                                index_row(session, &home),
                            )
                        })
                        .collect();
                    let mut waiting: Vec<(String, &'static str, String)> = rows
                        .iter()
                        .filter_map(|(id, status, row)| {
                            attention_of(status).map(|reason| (id.clone(), reason, row.clone()))
                        })
                        .collect();
                    sections.push(index_section(
                        rows.into_iter().map(|(id, _, row)| (id, row)).collect(),
                    ));
                    if let Some(drive) = service.runtime().and_then(|runtime| runtime.drive.clone())
                    {
                        match drive.records().await {
                            Ok(records) => {
                                waiting.extend(records.iter().filter_map(|record| {
                                    drive_attention(record).map(|reason| {
                                        (
                                            format!("drive:{}", record.thread_id),
                                            reason,
                                            record.line(),
                                        )
                                    })
                                }));
                                sections.push(drives_section(
                                    records
                                        .iter()
                                        .map(|record| (record.thread_id.clone(), record.line()))
                                        .collect(),
                                ));
                            }
                            Err(err) => {
                                tracing::warn!(
                                    "superagent: the Router's drive list is unavailable: {err:#}"
                                )
                            }
                        }
                    }
                    sections.push(attention_section(waiting));
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

/// The Router's rules for its level; told again when the level or the rules' text changes, or
/// history lost them. The snapshot is the level and a short digest of the text.
pub fn rules_section(mode: RouterMode) -> WorldStateSectionContribution {
    let digest = Sha256::digest(mode.rules().as_bytes());
    let snapshot = format!(
        "{}:{}",
        mode.name(),
        digest[..4]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    WorldStateSectionContribution::new(RULES_ID, move |previous| {
        let fragment = match previous {
            PreviousWorldStateSection::Known(Value::String(told)) if *told == snapshot => None,
            PreviousWorldStateSection::Known(_)
            | PreviousWorldStateSection::Unknown
            | PreviousWorldStateSection::Absent => Some(RenderedWorldStateFragment::new(
                "developer",
                RULES_MARKERS,
                input_text(mode.rules()),
            )),
        };
        (Some(Value::String(snapshot.clone())), fragment)
    })
    .with_retained_fragment_matcher(|role, text| starts_with_marker(role, text, RULES_MARKERS))
}

/// One index row: `id | title | directory | status | last activity | model/effort | account`.
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
    let model = match (session.model.as_deref(), session.effort.as_deref()) {
        (Some(model), Some(effort)) => format!("{model}/{effort}"),
        (Some(model), None) => model.to_owned(),
        (None, Some(effort)) => format!("-/{effort}"),
        (None, None) => "-".to_owned(),
    };
    format!(
        "{} | {title} | {cwd} | {} | {at} | {model} | {account}",
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
        "本机会话，按最近活动排序，最多 {INDEX_ROWS} 个（不含 Router 自己）。列：id | 名称或首条消息 | 目录 | 状态 | 最近活动 | 模型/强度 | 账号\n"
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

/// What a session waits for from the person, by its status: an error, an approval or an answer.
pub fn attention_of(status: &str) -> Option<&'static str> {
    if status == "systemError" {
        Some("出错")
    } else if status.contains("waitingOnApproval") {
        Some("等你批准")
    } else if status.contains("waitingOnUserInput") {
        Some("等你回答")
    } else {
        None
    }
}

/// The sessions that need the person. `waiting` is (id, reason, index row). The snapshot keeps
/// the reason by id, so each problem is told once, when it appears, and again when it is resolved.
pub fn attention_section(
    waiting: Vec<(String, &'static str, String)>,
) -> WorldStateSectionContribution {
    WorldStateSectionContribution::new(ATTENTION_ID, move |previous| {
        let snapshot: serde_json::Map<String, Value> = waiting
            .iter()
            .map(|(id, reason, _)| (id.clone(), Value::String((*reason).to_owned())))
            .collect();
        let line = |reason: &str, row: &str| format!("{reason}：{row}\n");
        let fragment = match previous {
            PreviousWorldStateSection::Known(Value::Object(before)) => {
                let mut text = String::new();
                for (id, reason, row) in &waiting {
                    if before.get(id).and_then(Value::as_str) != Some(*reason) {
                        text.push_str(&line(reason, row));
                    }
                }
                let resolved: Vec<&str> = before
                    .keys()
                    .map(String::as_str)
                    .filter(|id| !waiting.iter().any(|(waiting_id, _, _)| waiting_id == id))
                    .collect();
                if !resolved.is_empty() {
                    text.push_str(&format!("已不再等你：{}\n", resolved.join("、")));
                }
                (!text.is_empty()).then(|| {
                    RenderedWorldStateFragment::new(
                        "developer",
                        ATTENTION_UPDATE_MARKERS,
                        input_text(format!("需要你处理的会话有变化：\n{text}")),
                    )
                })
            }
            PreviousWorldStateSection::Known(_)
            | PreviousWorldStateSection::Unknown
            | PreviousWorldStateSection::Absent => (!waiting.is_empty()).then(|| {
                let mut text = String::from("需要你处理的会话（每个问题只列一次）：\n");
                for (_, reason, row) in &waiting {
                    text.push_str(&line(reason, row));
                }
                RenderedWorldStateFragment::new("developer", ATTENTION_MARKERS, input_text(text))
            }),
        };
        (Some(Value::Object(snapshot)), fragment)
    })
    .with_retained_fragment_matcher(|role, text| starts_with_marker(role, text, ATTENTION_MARKERS))
}
