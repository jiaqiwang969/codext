//! The engine's own round over the driven sessions, every `SWEEP_EVERY` (one effect row that
//! schedules itself again). It hands a parked session back when what it waits for came
//! (`wait.rs`), looks again at a step whose end was never read, and marks what someone watching
//! the drives would notice. The marks show in the session's line; the ones the Router should act
//! on go to it once, as a notice, when they appear.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use serde_json::json;

use super::DriveMode;
use super::DriveRecord;
use super::DriveService;
use super::DriveState;
use super::notices::Topic;
use super::progress;
use super::say::normalize;
use crate::effects::Effects;
use crate::effects::HandlerOptions;

/// The effect kind of the round, one row keyed `SWEEP_KEY`.
pub const KIND_SWEEP: &str = "drive-sweep";
const SWEEP_KEY: &str = "drives";
pub const SWEEP_EVERY: Duration = Duration::from_secs(60);
/// A conclusion nobody answered for this long is marked, and told.
const UNANSWERED: Duration = Duration::from_secs(30 * 60);
/// A step that runs this long while nothing is written in its folder for `STILL` is marked, and
/// told.
const LONG_STEP: Duration = Duration::from_secs(90 * 60);
const STILL: Duration = Duration::from_secs(60 * 60);
/// This many steps in a row, each handed over within `SHORT_STEP` of the one before, are small.
const SMALL_STEPS: usize = 5;
const SHORT_STEP: Duration = Duration::from_secs(3 * 60);
/// How many letters of two conclusions' openings match when the drive goes round in circles.
const OPENING: usize = 30;

/// Something about a session the round noticed.
struct Mark {
    text: String,
    /// What the Router is told to do, for a mark it is told about.
    hint: Option<&'static str>,
}

fn older_than(at: Option<&str>, age: Duration, now: DateTime<Utc>) -> bool {
    at.and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .is_some_and(|at| {
            now.signed_duration_since(at)
                .to_std()
                .is_ok_and(|elapsed| elapsed >= age)
        })
}

/// The folder a session works in.
fn folder(record: &DriveRecord) -> &std::path::Path {
    match record.mode {
        DriveMode::Anchored => record.project_home.parent().unwrap_or(&record.project_home),
        DriveMode::Simple => &record.project_home,
    }
}

/// The marks of `record` at `now`. `running` is how long the turn of its step runs, as its start
/// hook saw it; `crowded` names its account when that carries too many sessions.
fn marks(
    record: &DriveRecord,
    now: DateTime<Utc>,
    running: Option<Duration>,
    crowded: Option<&String>,
) -> Vec<Mark> {
    let mut marks = Vec::new();
    let acted = record.acted.as_deref();
    let changed = record
        .changed
        .as_deref()
        .or(record.report.as_ref().map(|report| report.at.as_str()));
    match &record.state {
        DriveState::Reported { .. }
            if older_than(changed, UNANSWERED, now)
                && (acted.is_none() || older_than(acted, UNANSWERED, now)) =>
        {
            marks.push(Mark {
                text: "交了结论半小时以上没人回".to_owned(),
                hint: Some("回它下一句；在等别的事就用 wait 写明等什么。"),
            });
        }
        DriveState::Driving if record.inflight.is_none() => {
            // Since the turn started; after a restart, since the step was handed over.
            let long = match running {
                Some(running) => running >= LONG_STEP,
                None => older_than(
                    record
                        .history
                        .iter()
                        .rev()
                        .find(|step| !step.steered)
                        .map(|step| step.at.as_str()),
                    LONG_STEP,
                    now,
                ),
            };
            if long {
                let still = progress::newest_write(folder(record)).is_none_or(|newest| {
                    SystemTime::now()
                        .duration_since(newest)
                        .is_ok_and(|quiet| quiet >= STILL)
                });
                if still {
                    marks.push(Mark {
                        text: "这一步做了一个半小时以上，文件夹一小时没有写入".to_owned(),
                        hint: Some(
                            "用 superagent_read session/<id> 看它卡在哪；卡住了就 interrupt，换个办法再说。",
                        ),
                    });
                }
            }
        }
        DriveState::Driving
        | DriveState::Reported { .. }
        | DriveState::Paused { .. }
        | DriveState::Deliverable { .. }
        | DriveState::NeedsDecision { .. } => {}
    }
    if record.max_steps > 0 && record.steps * 5 >= record.max_steps * 4 {
        marks.push(Mark {
            text: format!(
                "步数快用完（{}/{}），放宽要问用户",
                record.steps, record.max_steps
            ),
            hint: None,
        });
    }
    let opening = |text: &str| normalize(text).chars().take(OPENING).collect::<String>();
    if let (Some(report), Some(before)) = (&record.report, record.earlier.last())
        && opening(&report.text).chars().count() >= OPENING
        && opening(&report.text) == opening(before)
    {
        marks.push(Mark {
            text: "最近两次结论开头一样，可能在原地打转".to_owned(),
            hint: None,
        });
    }
    let handed: Vec<DateTime<Utc>> = record
        .history
        .iter()
        .filter(|step| !step.steered)
        .filter_map(|step| DateTime::parse_from_rfc3339(&step.at).ok())
        .map(|at| at.with_timezone(&Utc))
        .collect();
    let small = handed.len() > SMALL_STEPS
        && handed[handed.len() - SMALL_STEPS - 1..]
            .windows(2)
            .all(|pair| {
                pair[1]
                    .signed_duration_since(pair[0])
                    .to_std()
                    .is_ok_and(|gap| gap < SHORT_STEP)
            });
    if small {
        marks.push(Mark {
            text: format!(
                "连着 {SMALL_STEPS} 步都不到 3 分钟：一步交代一个完整的小目标，说清做到哪停"
            ),
            hint: None,
        });
    }
    if let Some(account) = crowded {
        marks.push(Mark {
            text: format!("账号 {account} 上的会话太多"),
            hint: None,
        });
    }
    marks
}

impl DriveService {
    /// One round: wake the sessions whose wait came, look again at steps whose end was never
    /// read, and mark what the round notices.
    pub async fn sweep(&self) -> anyhow::Result<()> {
        {
            let _lock = self.inner.lock.lock().await;
            self.look_at_waits().await?;
        }
        let records = self.records().await?;
        let crowded = self.crowded_sessions(&records).await?;
        let now = Utc::now();
        for record in &records {
            // The step's turn is over and its end was never read: read it.
            if record.state == DriveState::Driving
                && record.inflight.is_none()
                && let Ok(thread) = ThreadId::from_string(&record.thread_id)
            {
                match self.session_running(thread).await {
                    Ok(true) => {}
                    Ok(false) => self.schedule(&record.thread_id).await?,
                    Err(err) => tracing::warn!(
                        "superagent drive: the round could not see whether {} runs: {err:#}",
                        record.thread_id
                    ),
                }
            }
            let running = self
                .turn_seen(&record.thread_id)
                .filter(|seen| seen.ended.is_none())
                .and_then(|seen| seen.started)
                .map(|started| started.elapsed());
            let marks = marks(record, now, running, crowded.get(&record.thread_id));
            let texts: Vec<String> = marks.iter().map(|mark| mark.text.clone()).collect();
            if texts == record.marks {
                continue;
            }
            let told: Vec<&Mark> = marks
                .iter()
                .filter(|mark| mark.hint.is_some() && !record.marks.contains(&mark.text))
                .collect();
            self.change(&record.thread_id, |record| record.marks = texts)
                .await?;
            if !told.is_empty() {
                let mut text = format!(
                    "推进 · {} · 注意：{}\n会话 {}",
                    record.title,
                    told.iter()
                        .map(|mark| mark.text.as_str())
                        .collect::<Vec<_>>()
                        .join("；"),
                    record.thread_id
                );
                for hint in told.iter().filter_map(|mark| mark.hint) {
                    text.push_str(&format!("\n接下来：{hint}"));
                }
                self.queue_notice(record, Topic::Marks, text).await?;
            }
        }
        Ok(())
    }

    /// The driven sessions whose account carries more sessions at work than the policy allows,
    /// with that account. Empty when the rule is off.
    async fn crowded_sessions(
        &self,
        records: &[DriveRecord],
    ) -> anyhow::Result<HashMap<String, String>> {
        let cap = self.inner.policy.max_per_account;
        if cap == 0 {
            return Ok(HashMap::new());
        }
        let on = self.accounts_at_work(records).await;
        let mut load: HashMap<&String, usize> = HashMap::new();
        for account in on.values() {
            *load.entry(account).or_insert(0) += 1;
        }
        Ok(on
            .iter()
            .filter(|(thread, account)| {
                load.get(account).is_some_and(|sessions| *sessions > cap)
                    && records.iter().any(|record| record.thread_id == **thread)
            })
            .map(|(thread, account)| (thread.clone(), account.clone()))
            .collect())
    }

    /// Starts the round (one effect row that schedules itself again).
    pub async fn start_sweep(&self) -> anyhow::Result<()> {
        self.inner
            .effects
            .enqueue_now(KIND_SWEEP, SWEEP_KEY, &json!({}))
            .await
    }
}

/// Registers the round's handler: one round, then the next one is due `SWEEP_EVERY` later.
pub(super) async fn install_sweep_handler(
    service: &DriveService,
    effects: &Effects,
) -> anyhow::Result<()> {
    let sweeper = service.clone();
    effects
        .handle(
            KIND_SWEEP,
            Arc::new(move |_payload, _key| {
                let sweeper = sweeper.clone();
                Box::pin(async move {
                    if let Err(err) = sweeper.sweep().await {
                        tracing::warn!("superagent drive: the round over the drives: {err:#}");
                    }
                    if let Err(err) = sweeper
                        .inner
                        .effects
                        .enqueue_after(KIND_SWEEP, SWEEP_KEY, &json!({}), SWEEP_EVERY)
                        .await
                    {
                        tracing::warn!("superagent drive: the next round over the drives: {err:#}");
                    }
                    Ok(())
                })
            }),
            HandlerOptions::default().quiet(),
        )
        .await
}
