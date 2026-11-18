//! The drafter, for the sessions the Router turned it on for (`drafter`). A session's conclusion
//! goes first to a drafting session of its own (opened once, read-only, in the session's folder,
//! under the Router's account), which writes the Router's next message with its basis. The engine
//! checks that message like any `say` and sends it. The Router hears only what the drafter hands
//! up (a decision for the person, an outward action, a message the check refused, a draft that
//! failed or took too long) and, now and then, a summary of what went out in its name.
//!
//! A draft is an effect keyed by the session (`KIND_DRAFT`): the first run hands the drafter the
//! conclusion; the drafter's turn end runs it again to read the answer.

use std::sync::Arc;
use std::time::Duration;

use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;

use super::DriveMode;
use super::DriveRecord;
use super::DriveService;
use super::DriveState;
use super::Say;
use super::notices::Topic;
use super::report;
use crate::cards::truncate_chars;
use crate::core::now_iso;
use crate::effects::EffectError;
use crate::effects::Effects;
use crate::effects::HandlerOptions;
use crate::host::DeliveryMode;
use crate::host::ThreadSpec;

/// The effect kind of a draft, keyed by the driven session.
pub const KIND_DRAFT: &str = "drive-draft";
/// How long a draft may take before the conclusion goes to the Router instead.
const DRAFT_LIMIT: Duration = Duration::from_secs(10 * 60);
/// Messages the drafter sent are summed up for the Router once there are this many, or once the
/// oldest is `SUMMARY_AFTER` old.
const SUMMARY_EVERY: usize = 5;
const SUMMARY_AFTER: Duration = Duration::from_secs(60 * 60);
/// The role file a drafting session starts with, when the home has one.
const ROLE: &str = "drafter";

const RULES: &str = "\
你替总 Router 起草它对一个 worker 会话说的下一句话。每轮你会收到：用户的原话和纠正、长期规矩、验收清单（有的话）、最近几步说了什么、worker 刚交的结论。你只读不改：可以看文件和结果，不改任何东西，不运行会改动系统的命令。
- 先核对结论凭什么：它说通过了、推送了、修好了，看文件、提交号或运行结果；没核对的不当事实。
- 能在用户已给的范围内定的，写下一句：一两句人话，接着结论往下说，交代一个完整的小目标，说清做到哪停；不提方法名、skill 名和验收编号。
- builds_on 照抄你接着说的那段原话（结论、用户原话、纠正、长期规矩里的一段），serves 照抄用户原话、纠正或长期规矩里你为之服务的一段，都要一字不差。
- 这些交给 Router，不写下一句：做完了等用户验收；方向偏了；只有用户能定的取舍；要推送、发布、删除或对外发消息；同一件事已经两次不顺；你拿不准。
- 最后只输出一个 JSON 对象，不加别的：写下一句时 {\"decision\":\"say\",\"text\":\"…\",\"builds_on\":\"…\",\"serves\":\"…\",\"until\":\"…\"}；交给 Router 时 {\"decision\":\"ask\",\"why\":\"…\"}。";

/// A draft in progress for a session's conclusion.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Drafting {
    /// The turn of the conclusion being drafted for.
    pub turn: String,
    /// When the draft began, RFC 3339.
    pub since: String,
    /// The drafting session's latest turn before this draft went to it, once it went; the
    /// answer comes from a later one.
    pub sent_after: Option<Option<String>>,
}

/// A message the drafter sent, for the Router's next summary.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Drafted {
    pub at: String,
    pub step: u32,
    pub text: String,
}

/// What the drafter answered.
#[derive(Debug, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
enum Answer {
    Say {
        text: String,
        builds_on: String,
        serves: String,
        #[serde(default)]
        until: Option<String>,
    },
    Ask {
        why: String,
    },
}

/// The JSON object at the end of the drafter's last message.
fn answer_of(message: &str) -> Option<Answer> {
    let start = message.find('{')?;
    let end = message.rfind('}')?;
    serde_json::from_str(message.get(start..=end)?).ok()
}

fn older_than(at: &str, age: Duration) -> bool {
    DateTime::parse_from_rfc3339(at).is_ok_and(|at| {
        Utc::now()
            .signed_duration_since(at)
            .to_std()
            .is_ok_and(|elapsed| elapsed >= age)
    })
}

/// What the drafter is told about the session's conclusion.
fn brief(record: &DriveRecord, standing: &[String], acceptance: &[String]) -> String {
    let mut text = format!(
        "会话：{}（{}）\n用户的原话：{}\n",
        record.title, record.thread_id, record.request
    );
    if !record.corrections.is_empty() {
        text.push_str(&format!(
            "用户后来的纠正：{}\n",
            record.corrections.join(" / ")
        ));
    }
    if !standing.is_empty() {
        text.push_str(&format!("长期规矩：{}\n", standing.join(" / ")));
    }
    if !acceptance.is_empty() {
        text.push_str(&format!("验收清单：{}\n", acceptance.join(" / ")));
    }
    text.push_str("最近几步：\n");
    for step in record.history.iter().rev().take(3).rev() {
        text.push_str(&format!(
            "- 第 {} 步：{}\n",
            step.n,
            truncate_chars(&step.text, 300)
        ));
    }
    if let Some(report) = &record.report {
        text.push_str(&format!(
            "worker 刚交的结论（第 {} 步）：\n{}\n",
            record.steps, report.text
        ));
        if let Some(audit) = &report.audit {
            text.push_str(&format!("判定：{audit}\n"));
        }
    }
    if !record.notes.is_empty() {
        text.push_str(&format!("用户在会话里说过：{}\n", record.notes.join(" / ")));
    }
    text.push_str(&format!(
        "已推进 {}/{} 步。写下一句，或交给 Router；最后只输出 JSON。",
        record.steps, record.max_steps
    ));
    text
}

impl DriveService {
    /// Turns the drafter on or off for a session.
    pub async fn set_drafter(
        &self,
        thread_id: &str,
        on: bool,
    ) -> anyhow::Result<Option<DriveRecord>> {
        self.change(thread_id, |record| {
            record.drafter = on;
            if !on {
                record.drafting = None;
            }
            record.clone()
        })
        .await
    }

    /// The drafting sessions, which the Router's index leaves out.
    pub async fn drafter_threads(&self) -> anyhow::Result<Vec<String>> {
        Ok(self
            .records()
            .await?
            .into_iter()
            .filter_map(|record| record.drafter_thread)
            .collect())
    }

    /// The driven session whose drafting session `thread_id` is.
    pub(super) async fn drafted_session(&self, thread_id: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .records()
            .await?
            .into_iter()
            .find(|record| record.drafter_thread.as_deref() == Some(thread_id))
            .map(|record| record.thread_id))
    }

    /// Hands the conclusion of `record` (now `told`) on: to the drafter when it is on for the
    /// session and the conclusion is the Router's to answer, else to the Router. Called under the
    /// lock, after the conclusion is recorded.
    pub(super) async fn hand_on(
        &self,
        record: &DriveRecord,
        told: &DriveState,
    ) -> anyhow::Result<()> {
        let drafts = record.drafter && matches!(told, DriveState::Reported { .. });
        let turn = record.report.as_ref().map(|report| report.turn.clone());
        let Some(turn) = turn.filter(|_| drafts) else {
            return self.notify(record, told).await;
        };
        let drafting = Drafting {
            turn,
            since: now_iso(),
            sent_after: None,
        };
        self.change_locked(&record.thread_id, |record| record.drafting = Some(drafting))
            .await?;
        self.inner
            .effects
            .enqueue_now(KIND_DRAFT, &record.thread_id, &json!({}))
            .await
    }

    /// One run of a session's draft: hands the drafter the conclusion, or reads its answer and
    /// acts on it. A draft whose session moved on meanwhile (the Router or the person spoke) is
    /// dropped.
    pub async fn draft(&self, thread_id: &str) -> Result<(), EffectError> {
        let failed = |err: anyhow::Error| EffectError::Failed(format!("{err:#}"));
        let Some(record) = self.record(thread_id).await.map_err(failed)? else {
            return Ok(());
        };
        let Some(drafting) = record.drafting.clone() else {
            return Ok(());
        };
        let current = record.report.as_ref().map(|report| report.turn.as_str());
        if current != Some(drafting.turn.as_str())
            || !matches!(record.state, DriveState::Reported { .. })
        {
            self.change(thread_id, |record| record.drafting = None)
                .await
                .map_err(failed)?;
            return Ok(());
        }
        match &drafting.sent_after {
            None => self.send_draft(&record).await.map_err(failed),
            Some(after) => self.read_draft(&record, after.as_deref()).await,
        }
    }

    /// Hands the drafter the session's conclusion, opening the drafting session the first time.
    async fn send_draft(&self, record: &DriveRecord) -> anyhow::Result<()> {
        let drafter = match &record.drafter_thread {
            Some(thread) => ThreadId::from_string(thread)?,
            None => match self.open_drafter(record).await {
                Ok(thread) => thread,
                Err(err) => {
                    return self
                        .draft_failed(record, &format!("开不了起草会话：{err:#}"))
                        .await;
                }
            },
        };
        let before = self
            .inner
            .host
            .session_turns(drafter, 1)
            .await?
            .first()
            .map(|turn| turn.id.clone());
        let standing = self.standing_for(record).await?;
        let acceptance = match record.mode {
            DriveMode::Anchored => super::Anchor::read(&record.project_home)
                .unwrap_or_default()
                .acceptance
                .into_iter()
                .map(|(_, statement, status)| format!("{statement}（{status}）"))
                .collect(),
            DriveMode::Simple => Vec::new(),
        };
        let text = brief(record, &standing, &acceptance);
        let drafter_id = drafter.to_string();
        self.change(&record.thread_id, |record| {
            record.drafter_thread = Some(drafter_id);
            if let Some(drafting) = record.drafting.as_mut() {
                drafting.sent_after = Some(before);
            }
        })
        .await?;
        if let Err(err) = self
            .inner
            .host
            .deliver(drafter, text, DeliveryMode::Queue)
            .await
        {
            return self
                .draft_failed(record, &format!("起草会话收不到结论：{err:#}"))
                .await;
        }
        Ok(())
    }

    /// Opens the read-only drafting session of `record`, in its folder, under the account of
    /// the Router that drives it; with the drafter role file when the home has one.
    async fn open_drafter(&self, record: &DriveRecord) -> anyhow::Result<ThreadId> {
        let profile = match self
            .router_for(record)
            .map(|router| ThreadId::from_string(&router))
        {
            Some(Ok(router)) => self.inner.host.thread_profile(router).await?,
            Some(Err(_)) | None => None,
        };
        let folder = match record.mode {
            DriveMode::Anchored => record
                .project_home
                .parent()
                .unwrap_or(&record.project_home)
                .to_owned(),
            DriveMode::Simple => record.project_home.clone(),
        };
        let spec = ThreadSpec {
            cwd: folder,
            profile,
            role: Some(ROLE.to_owned()),
            developer_instructions: Some(RULES.to_owned()),
            read_only: true,
            ..ThreadSpec::default()
        };
        match self.inner.host.start_thread(spec.clone()).await {
            Ok(thread) => Ok(thread),
            // Without the role file the session starts on the home's default model.
            Err(err) => {
                tracing::info!("superagent drive: a drafter without its role file ({err:#})");
                self.inner
                    .host
                    .start_thread(ThreadSpec { role: None, ..spec })
                    .await
            }
        }
    }

    /// Reads the drafter's answer, once its turn after `after` has ended, and acts on it.
    async fn read_draft(
        &self,
        record: &DriveRecord,
        after: Option<&str>,
    ) -> Result<(), EffectError> {
        let failed = |err: anyhow::Error| EffectError::Failed(format!("{err:#}"));
        let Some(drafter) = record
            .drafter_thread
            .as_deref()
            .and_then(|thread| ThreadId::from_string(thread).ok())
        else {
            return self
                .draft_failed(record, "找不到起草会话")
                .await
                .map_err(failed);
        };
        let turns = self
            .inner
            .host
            .session_turns(drafter, 1)
            .await
            .map_err(failed)?;
        let Some(turn) = turns.first().filter(|turn| Some(turn.id.as_str()) != after) else {
            return Err(EffectError::Later("the drafter has not started".to_owned()));
        };
        if turn.status == "inProgress" {
            return Err(EffectError::Later("the drafter is writing".to_owned()));
        }
        let answer = (turn.status == "completed")
            .then(|| answer_of(&report::conclusion(turn)))
            .flatten();
        let result = match answer {
            Some(Answer::Say {
                text,
                builds_on,
                serves,
                until,
            }) => {
                self.change(&record.thread_id, |record| record.drafting = None)
                    .await
                    .map_err(failed)?;
                let say = Say {
                    text: text.clone(),
                    builds_on,
                    serves,
                    until,
                    ..Say::default()
                };
                match self.say(&record.thread_id, say).await {
                    Ok(_) => {
                        let drafted = Drafted {
                            at: now_iso(),
                            step: record.steps + 1,
                            text: truncate_chars(&text, 200),
                        };
                        self.change(&record.thread_id, |record| record.drafted.push(drafted))
                            .await
                            .map(drop)
                    }
                    Err(err) => {
                        self.hand_up(
                            record,
                            &format!(
                                "起草者写的下一句被退回（{}）：「{}」",
                                truncate_chars(&format!("{err:#}"), 160),
                                truncate_chars(&text, 200)
                            ),
                        )
                        .await
                    }
                }
            }
            Some(Answer::Ask { why }) => {
                self.change(&record.thread_id, |record| record.drafting = None)
                    .await
                    .map_err(failed)?;
                self.hand_up(record, &format!("起草者认为这一步要你处理：{why}"))
                    .await
            }
            None => self.draft_failed(record, "起草者没给出能用的答复").await,
        };
        result.map_err(failed)
    }

    /// The draft did not work out: the conclusion goes to the Router as it would without one.
    async fn draft_failed(&self, record: &DriveRecord, why: &str) -> anyhow::Result<()> {
        self.change(&record.thread_id, |record| record.drafting = None)
            .await?;
        self.hand_up(record, &format!("（起草没成：{why}）")).await
    }

    /// The session's conclusion goes to the Router, with what the drafter adds.
    async fn hand_up(&self, record: &DriveRecord, note: &str) -> anyhow::Result<()> {
        let Some(notice) = report::notice(record, &record.state) else {
            return Ok(());
        };
        self.queue_notice(record, Topic::State, format!("{notice}\n{note}"))
            .await
    }

    /// The round's part: a draft running too long goes to the Router, and what the drafter sent
    /// is summed up for the Router once there is enough of it or it is old enough.
    pub(super) async fn look_at_drafts(&self, records: &[DriveRecord]) -> anyhow::Result<()> {
        for record in records {
            if let Some(drafting) = &record.drafting
                && older_than(&drafting.since, DRAFT_LIMIT)
            {
                self.draft_failed(record, "超过十分钟").await?;
            }
        }
        let drafted: Vec<(&DriveRecord, &Drafted)> = records
            .iter()
            .flat_map(|record| record.drafted.iter().map(move |note| (record, note)))
            .collect();
        let due = drafted.len() >= SUMMARY_EVERY
            || drafted
                .iter()
                .any(|(_, note)| older_than(&note.at, SUMMARY_AFTER));
        if !due {
            return Ok(());
        }
        let mut text = format!("[通知] 起草者这段时间替你回了 {} 句：", drafted.len());
        for (record, note) in &drafted {
            text.push_str(&format!(
                "\n- {}（{}）第 {} 步：{}",
                record.title, record.thread_id, note.step, note.text
            ));
        }
        self.tell_router(&format!("drafted:{}", now_iso()), text)
            .await?;
        for record in records.iter().filter(|record| !record.drafted.is_empty()) {
            self.change(&record.thread_id, |record| record.drafted.clear())
                .await?;
        }
        Ok(())
    }
}

/// Registers the drafts' handler.
pub(super) async fn install_draft_handler(
    service: &DriveService,
    effects: &Effects,
) -> anyhow::Result<()> {
    let drafter = service.clone();
    effects
        .handle(
            KIND_DRAFT,
            Arc::new(move |_payload, thread_id| {
                let drafter = drafter.clone();
                Box::pin(async move { drafter.draft(&thread_id).await })
            }),
            HandlerOptions::default().retry(3_000, 30_000),
        )
        .await
}
