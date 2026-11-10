//! The Router answers a driven session (`say`): one message in plain words, with its basis — the
//! words of the worker's last conclusion (or the person's) it follows from, and the words of the
//! person's request it serves. The engine checks the basis against its own records before the
//! message goes out, so a Router that misremembers or lost its context after compaction cannot
//! send a step detached from what happened: a quote must really be there, the message must not
//! repeat the last ones, must not recite method names or item ids, and stays within the step cap.
//!
//! The person's words to the Router are logged as they arrive (`heard`), so a correction the
//! Router passes on can be checked against what the person actually said.

use codex_protocol::ThreadId;

use super::Anchor;
use super::DriveMode;
use super::DriveRecord;
use super::DriveService;
use super::DriveState;
use super::NOTES;
use super::Step;
use super::drive_key;
use super::push_limited;
use crate::cards::truncate_chars;
use crate::core::now_iso;
use crate::host::DeliveryMode;

/// The ledger key of the person's latest words to the Router.
const HEARD_KEY: &str = "drive-heard";
const HEARD: usize = 30;
/// The longest message a step may carry; longer material goes into files the worker reads.
const SPOKEN_CHARS: usize = 1_500;
/// The shortest quote that counts as basis, unless it is a whole source.
const MIN_QUOTE: usize = 4;
/// Method names and record words that belong to the engine's thinking, not to what the worker
/// is told (compared after `normalize`).
const JARGON: &[&str] = &[
    "验收项",
    "routeaudit",
    "routeanchor",
    "goalanchoring",
    "工程本体",
    "阶段声明",
    "锚定记录",
    "ontology",
    "hdae",
    "superagent",
    "oev2",
];

/// What the Router tells a driven session, and what it follows from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Say {
    /// The message, in plain words.
    pub text: String,
    /// Words copied from the worker's last conclusion, the person's request or a correction.
    pub builds_on: String,
    /// Words copied from the person's request (or a correction, or an acceptance item).
    pub serves: String,
    /// The person's words to the Router this message passes on, verbatim; kept with the goal.
    pub correction: Option<String>,
}

/// Letters and digits only, lowercased: quotes survive changed spacing and punctuation.
fn normalize(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether `quote` is copied from one of `sources`.
fn found_in<'a>(quote: &str, sources: impl IntoIterator<Item = &'a str>) -> bool {
    let quote = normalize(quote);
    if quote.is_empty() {
        return false;
    }
    let long_enough = quote.chars().count() >= MIN_QUOTE;
    sources.into_iter().any(|source| {
        let source = normalize(source);
        !source.is_empty() && source.contains(&quote) && (long_enough || source == quote)
    })
}

/// An acceptance item id (`A1`) standing on its own.
fn item_id(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    (0..chars.len().saturating_sub(1)).find_map(|i| {
        let alone = i == 0 || !chars[i - 1].is_ascii_alphanumeric();
        (alone && chars[i] == 'A' && chars[i + 1].is_ascii_digit()).then(|| {
            chars[i..]
                .iter()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect()
        })
    })
}

/// Checks a message for the session: present, not too long, in plain words.
pub fn spoken(text: &str) -> anyhow::Result<String> {
    let text = text.trim();
    anyhow::ensure!(!text.is_empty(), "要对会话说的话（text）是空的");
    anyhow::ensure!(
        text.chars().count() <= SPOKEN_CHARS,
        "一步的话不超过 {SPOKEN_CHARS} 字；长的材料写进文件，话里给路径"
    );
    let plain = normalize(text);
    let jargon: Vec<&str> = JARGON
        .iter()
        .copied()
        .filter(|word| plain.contains(word))
        .chain(item_id(text).as_deref().map(|_| "验收编号"))
        .collect();
    anyhow::ensure!(
        jargon.is_empty(),
        "话里有方法名或编号（{}）：这些是你的思路，不说给 worker；用人话说要做什么、做完看到什么",
        jargon.join("、")
    );
    Ok(text.to_owned())
}

impl DriveService {
    /// Logs what the person said to the Router, so a correction passed on can be checked.
    pub async fn heard(&self, text: &str) -> anyhow::Result<()> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        let text = text.to_owned();
        self.inner
            .store
            .atomic(async |tx| {
                let mut heard: Vec<String> = tx.kv_get(HEARD_KEY).await?.unwrap_or_default();
                push_limited(&mut heard, text, HEARD);
                tx.kv_set(HEARD_KEY, &heard).await
            })
            .await
    }

    /// Fails unless `words` are (part of) something the person said to the Router.
    pub(super) async fn ensure_heard(&self, words: &str) -> anyhow::Result<()> {
        let heard: Vec<String> = self
            .inner
            .store
            .kv_get(HEARD_KEY)
            .await?
            .unwrap_or_default();
        anyhow::ensure!(
            found_in(words, heard.iter().map(String::as_str)),
            "用户最近没有对你说过这段话：correction / text 要照抄用户的原话"
        );
        Ok(())
    }

    /// Sends the Router's next message to a driven session after checking its basis.
    pub async fn say(&self, thread_id: &str, say: Say) -> anyhow::Result<Option<DriveRecord>> {
        let thread = ThreadId::from_string(thread_id)
            .map_err(|err| anyhow::anyhow!("会话 id {thread_id} 无效：{err}"))?;
        let text = spoken(&say.text)?;
        let correction = say
            .correction
            .as_deref()
            .map(str::trim)
            .filter(|words| !words.is_empty())
            .map(str::to_owned);
        if let Some(words) = &correction {
            self.ensure_heard(words).await?;
        }
        let lock = self.inner.lock.lock().await;
        let Some(record) = self
            .inner
            .store
            .kv_get::<DriveRecord>(&drive_key(thread_id))
            .await?
        else {
            return Ok(None);
        };
        // While the step's turn runs, the message is said into it (the person corrected course);
        // the worker reads it at its next move. Otherwise it starts the next step.
        let steering = match record.state {
            DriveState::Driving => {
                anyhow::ensure!(
                    record.inflight.is_none(),
                    "第 {} 步刚交出、还没开始，稍等再说",
                    record.steps
                );
                if !self.session_running(thread).await? {
                    // Its end is being read; should that look have gone missing, this one
                    // reads it.
                    self.schedule(thread_id).await?;
                    anyhow::bail!("第 {} 步刚结束，正在读它的结论，稍等再说", record.steps);
                }
                true
            }
            DriveState::Reported { .. }
            | DriveState::Paused { .. }
            | DriveState::Deliverable { .. }
            | DriveState::NeedsDecision { .. } => false,
        };
        anyhow::ensure!(
            steering || record.steps < record.max_steps,
            "已推进 {}/{} 步：要继续先问用户，同意后用 resume 带 max_steps 放宽",
            record.steps,
            record.max_steps
        );
        let mut corrections = record.corrections.clone();
        if let Some(words) = &correction
            && !corrections.contains(words)
        {
            push_limited(&mut corrections, words.clone(), NOTES);
        }
        let conclusion = record.report.as_ref().map(|report| report.text.as_str());
        let mut grounds: Vec<&str> = conclusion.into_iter().collect();
        grounds.push(&record.request);
        grounds.extend(corrections.iter().map(String::as_str));
        grounds.extend(record.notes.iter().map(String::as_str));
        anyhow::ensure!(
            found_in(&say.builds_on, grounds),
            "builds_on 要照抄 worker 最近结论里的一段原话（或用户的原话、纠正），表明这句话接着什么说；没有找到「{}」",
            truncate_chars(say.builds_on.trim(), 60)
        );
        let anchor = match record.mode {
            DriveMode::Anchored => Anchor::read(&record.project_home).unwrap_or_default(),
            DriveMode::Simple => Anchor::default(),
        };
        let mut purposes: Vec<&str> = vec![record.request.as_str(), anchor.deliverable.as_str()];
        purposes.extend(corrections.iter().map(String::as_str));
        purposes.extend(
            anchor
                .acceptance
                .iter()
                .map(|(_, statement, _)| statement.as_str()),
        );
        purposes.extend(anchor.corrections.iter().map(|(_, text, _)| text.as_str()));
        anyhow::ensure!(
            found_in(&say.serves, purposes),
            "serves 要照抄用户原话（或纠正、验收清单）里的一段，表明这一步为了源头的哪一点；没有找到「{}」",
            truncate_chars(say.serves.trim(), 60)
        );
        if let Some(previous) = record
            .history
            .iter()
            .rev()
            .take(2)
            .find(|step| normalize(&step.text) == normalize(&text))
        {
            anyhow::bail!(
                "和第 {} 步说的一样：重试要有新的依据或换个办法；拿不准就问用户",
                previous.n
            );
        }
        let n = if steering {
            record.steps
        } else {
            record.steps + 1
        };
        let step = Step {
            n,
            at: now_iso(),
            audit: record
                .report
                .as_ref()
                .and_then(|report| report.audit.clone())
                .unwrap_or_default(),
            text: text.clone(),
            builds_on: truncate_chars(say.builds_on.trim(), 300),
            serves: truncate_chars(say.serves.trim(), 300),
            steered: steering,
        };
        let changed = self
            .change_locked(thread_id, |record| {
                if !steering {
                    record.steps = n;
                    record.inflight = Some(n);
                    record.state = DriveState::Driving;
                    record.notes.clear();
                }
                record.corrections = corrections;
                record.history.push(step);
                if record.history.len() > super::HISTORY {
                    record.history.remove(0);
                }
                record.clone()
            })
            .await?;
        drop(lock);
        if steering {
            // Outside the lock: the host reports the steered input back through the intake.
            if let Err(err) = self
                .inner
                .host
                .deliver(thread, text, DeliveryMode::Steer)
                .await
            {
                self.change(thread_id, |record| {
                    if record.history.last().is_some_and(|step| step.steered) {
                        record.history.pop();
                    }
                })
                .await?;
                return Err(err.context(format!("没能插进第 {n} 步")));
            }
        } else {
            self.hand_over(thread_id, thread, &record.title, n, text)
                .await?;
        }
        match changed {
            Some(_) => self.record(thread_id).await,
            None => Ok(None),
        }
    }
}
