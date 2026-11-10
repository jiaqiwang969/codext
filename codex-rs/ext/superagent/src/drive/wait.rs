//! A session the Router parks on purpose (`wait`): what it waits for is kept on the record, so
//! neither the Router's compacted memory nor a restart loses it. The engine's round (`sweep.rs`)
//! sees when it came (another session's next conclusion, a time, a file) and hands the session
//! back to the Router with a notice; a wait that runs past its reminder is told once.

use std::path::PathBuf;
use std::time::SystemTime;

use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

use super::DriveRecord;
use super::DriveService;
use super::DriveState;
use super::notices::Topic;
use crate::cards::truncate_chars;
use crate::core::now_iso;

/// What the reason of a parked session starts with.
pub const WAITING: &str = "等待：";

/// What a parked session waits for.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitFor {
    /// The next conclusion of another driven session: one other than `seen`, the turn of its
    /// conclusion when the wait began.
    Session {
        thread: String,
        seen: Option<String>,
    },
    /// A time, RFC 3339.
    Time { at: String },
    /// A file that appears, or changes after `seen` (its modification time when the wait began,
    /// RFC 3339).
    File { path: PathBuf, seen: Option<String> },
    /// The person's answer: nothing the engine sees; only the reminder goes.
    Person,
}

/// A wait on a session's record.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Wait {
    /// The Router's words for what the session waits for.
    pub what: String,
    pub on: WaitFor,
    pub since: String,
    /// When the Router hears that it still waits, RFC 3339; `None` for no reminder.
    pub remind: Option<String>,
    #[serde(default)]
    pub reminded: bool,
}

/// What the Router asks a session to wait for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitOn {
    /// Another driven session's next conclusion.
    Session(String),
    /// Some minutes.
    Minutes(u32),
    /// A file, by its absolute path.
    File(PathBuf),
    /// The person's answer.
    Person,
}

fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn modified(path: &std::path::Path) -> Option<String> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .map(|at: SystemTime| rfc3339(at.into()))
}

fn passed(at: &str, now: DateTime<Utc>) -> bool {
    DateTime::parse_from_rfc3339(at).is_ok_and(|at| at <= now)
}

/// `at` (RFC 3339) as the person's clock reads it.
fn local(at: &str) -> String {
    DateTime::parse_from_rfc3339(at).map_or_else(
        |_| at.to_owned(),
        |at| {
            at.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string()
        },
    )
}

impl WaitFor {
    /// Whether what the wait is for came, given the records of the drives; answers what came.
    fn came(&self, records: &[DriveRecord], now: DateTime<Utc>) -> Option<String> {
        match self {
            Self::Session { thread, seen } => {
                let Some(other) = records.iter().find(|record| record.thread_id == *thread) else {
                    return Some(format!("会话 {thread} 已不再推进"));
                };
                let report = other.report.as_ref()?;
                (seen.as_deref() != Some(report.turn.as_str())).then(|| {
                    format!(
                        "{}（{thread}）交了新结论：{}",
                        other.title,
                        truncate_chars(report.text.trim(), 200)
                    )
                })
            }
            Self::Time { at } => passed(at, now).then(|| "到时间了".to_owned()),
            Self::File { path, seen } => {
                let now_modified = modified(path)?;
                let changed = match seen {
                    Some(seen) => DateTime::parse_from_rfc3339(seen)
                        .ok()
                        .zip(DateTime::parse_from_rfc3339(&now_modified).ok())
                        .is_some_and(|(seen, now_modified)| now_modified > seen),
                    None => true,
                };
                changed.then(|| format!("文件 {} 有了", path.display()))
            }
            Self::Person => None,
        }
    }
}

impl DriveService {
    /// Parks the session until what it waits for comes: the drive pauses with the reason, and
    /// the engine's round hands it back. `remind_after` (minutes) tells the Router once when
    /// nothing came; for a wait of some minutes the minutes are the wait itself.
    pub async fn wait(
        &self,
        thread_id: &str,
        what: &str,
        on: WaitOn,
        remind_after: Option<u32>,
    ) -> anyhow::Result<Option<DriveRecord>> {
        let what = what.trim().to_owned();
        anyhow::ensure!(!what.is_empty(), "要写明等什么（text）");
        let now = Utc::now();
        let minutes_from_now =
            |minutes: u32| rfc3339(now + chrono::Duration::minutes(i64::from(minutes)));
        let on = match on {
            WaitOn::Session(thread) => {
                anyhow::ensure!(thread != thread_id, "不能等自己这一路");
                let other = self.record(&thread).await?.ok_or_else(|| {
                    anyhow::anyhow!("会话 {thread} 不在推进记录里：只能等推进中的会话交结论")
                })?;
                WaitFor::Session {
                    thread,
                    seen: other.report.map(|report| report.turn),
                }
            }
            WaitOn::Minutes(minutes) => {
                anyhow::ensure!(minutes > 0, "等几分钟（minutes）要大于 0");
                WaitFor::Time {
                    at: minutes_from_now(minutes),
                }
            }
            WaitOn::File(path) => {
                anyhow::ensure!(path.is_absolute(), "文件要写绝对路径：{}", path.display());
                let seen = modified(&path);
                WaitFor::File { path, seen }
            }
            WaitOn::Person => WaitFor::Person,
        };
        let remind = match on {
            WaitFor::Time { .. } => None,
            WaitFor::Session { .. } | WaitFor::File { .. } | WaitFor::Person => remind_after
                .filter(|minutes| *minutes > 0)
                .map(minutes_from_now),
        };
        let wait = Wait {
            what: truncate_chars(&what, 200),
            on,
            since: now_iso(),
            remind,
            reminded: false,
        };
        let changed = self
            .change(thread_id, |record| {
                if record.state == DriveState::Driving {
                    return Err(anyhow::anyhow!(
                        "第 {} 步还在做：等它做完再停放；要它马上停用 interrupt",
                        record.steps
                    ));
                }
                record.state = DriveState::Paused {
                    reason: format!("{WAITING}{}", wait.what),
                };
                record.wait = Some(wait);
                record.acted = Some(now_iso());
                record.refused = None;
                Ok(record.clone())
            })
            .await?;
        changed.transpose()
    }

    /// One look at every parked session, under the lock the caller holds: one whose wait came
    /// goes back to the Router with a notice; one past its reminder is told once.
    pub(super) async fn look_at_waits(&self) -> anyhow::Result<()> {
        let records = self.records().await?;
        let now = Utc::now();
        for record in &records {
            let Some(wait) = &record.wait else {
                continue;
            };
            if let Some(came) = wait.on.came(&records, now) {
                let summary = format!("等的东西到了：{}", wait.what);
                let state = DriveState::Reported {
                    summary: summary.clone(),
                };
                let woke = self
                    .change_locked(&record.thread_id, |record| {
                        // The Router spoke to the session meanwhile: nothing to wake.
                        if record.wait.is_none() {
                            return None;
                        }
                        record.wait = None;
                        record.state = state;
                        Some(record.clone())
                    })
                    .await?
                    .flatten();
                if let Some(record) = woke {
                    let text = format!(
                        "推进 · {} · {summary}\n会话 {}\n{came}\n接下来：用 superagent_drive say 接着说。",
                        record.title, record.thread_id
                    );
                    self.queue_notice(&record, Topic::State, text).await?;
                }
                continue;
            }
            let overdue = !wait.reminded
                && wait
                    .remind
                    .as_deref()
                    .is_some_and(|remind| passed(remind, now));
            if overdue {
                let reminded = self
                    .change_locked(&record.thread_id, |record| {
                        let Some(wait) = record.wait.as_mut() else {
                            return false;
                        };
                        wait.reminded = true;
                        true
                    })
                    .await?
                    .unwrap_or(false);
                if reminded {
                    let text = format!(
                        "推进 · {} · 还在等：{}（{} 开始等的）\n会话 {}\n接下来：去看看等的事怎么样了；等不到就换办法，或问用户。",
                        record.title,
                        wait.what,
                        local(&wait.since),
                        record.thread_id
                    );
                    self.queue_notice(record, Topic::State, text).await?;
                }
            }
        }
        Ok(())
    }
}
