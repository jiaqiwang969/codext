//! The Router answers a driven session (`say`): one message in plain words, with its basis — the
//! words of the worker's conclusions (or the person's) it follows from, and the words of the
//! person's request it serves (where quotes may come from: `basis.rs`). The engine checks the
//! basis against its own records before the message goes out, so a Router that misremembers or
//! lost its context after compaction cannot send a step detached from what happened: a quote must
//! really be there, the message must not repeat the last ones, must not recite method names or
//! item ids, and stays within the step cap. A drive the engine stopped for the person goes on
//! only with the person's words.
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
use super::basis::Found;
use super::basis::conclusion_opening;
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
    /// Words copied from the worker's last conclusions, the person's request or a correction,
    /// what the person told the Router, a standing rule, or another session's conclusions.
    pub builds_on: String,
    /// Words copied from the person's request (or a correction, an acceptance item, what the
    /// person told the Router, a standing rule).
    pub serves: String,
    /// The person's words to the Router this message passes on, verbatim; kept with the goal.
    pub correction: Option<String>,
    /// Where the step should stop, in plain words; told to the worker after the message.
    pub until: Option<String>,
    /// Raises the most steps the drive may send (the person agreed); never below the steps sent.
    pub max_steps: Option<u32>,
}

/// How the end of a step is told to the worker, after the message.
const UNTIL: &str = "这一步做到这里停：";
/// How much of a refusal the record keeps.
const REFUSED_CHARS: usize = 200;

/// Letters and digits only, lowercased: quotes survive changed spacing and punctuation.
pub(super) fn normalize(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whether `quote` is copied from one of `sources`.
pub(super) fn found_in<'a>(quote: &str, sources: impl IntoIterator<Item = &'a str>) -> bool {
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

    /// What the person told the Router lately, oldest first.
    pub(super) async fn heard_words(&self) -> anyhow::Result<Vec<String>> {
        Ok(self
            .inner
            .store
            .kv_get(HEARD_KEY)
            .await?
            .unwrap_or_default())
    }

    /// Fails unless `words` are (part of) something the person said to the Router.
    pub(super) async fn ensure_heard(&self, words: &str) -> anyhow::Result<()> {
        let heard = self.heard_words().await?;
        anyhow::ensure!(
            found_in(words, heard.iter().map(String::as_str)),
            "用户最近没有对你说过这段话：correction / text 要照抄用户的原话"
        );
        Ok(())
    }

    /// Sends the Router's next message to a driven session after checking its basis. A message
    /// refused while the session waits for one is noted on its record, so the Router's list shows
    /// the session still waits until a message goes out.
    pub async fn say(&self, thread_id: &str, say: Say) -> anyhow::Result<Option<DriveRecord>> {
        let sent = self.send(thread_id, say).await;
        if let Err(err) = &sent {
            let reason = truncate_chars(&format!("{err:#}"), REFUSED_CHARS);
            let noted = self
                .change(thread_id, |record| {
                    if record.state != DriveState::Driving {
                        record.refused = Some(reason);
                    }
                })
                .await;
            if let Err(note) = noted {
                tracing::warn!("superagent drive: refusal of {thread_id} not noted: {note:#}");
            }
        }
        sent
    }

    async fn send(&self, thread_id: &str, say: Say) -> anyhow::Result<Option<DriveRecord>> {
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
            // The engine stopped for the person (drift, no progress at all): the drive goes on
            // with the person's answer only.
            DriveState::NeedsDecision { ref reason } => {
                anyhow::ensure!(
                    correction.is_some(),
                    "这一路停下来是要用户定的事（{}）：先问用户，用户回话后再 say，correction 照抄用户的原话",
                    truncate_chars(reason, 80)
                );
                false
            }
            DriveState::Reported { .. }
            | DriveState::Paused { .. }
            | DriveState::Deliverable { .. } => false,
        };
        // The person agreed to more steps: the cap rises with this message.
        let max_steps = say
            .max_steps
            .filter(|n| *n > 0)
            .map_or(record.max_steps, |n| n.max(record.steps));
        anyhow::ensure!(
            steering || record.steps < max_steps,
            "已推进 {}/{} 步：要继续先问用户，同意后带 max_steps 放宽",
            record.steps,
            max_steps
        );
        let text = match say
            .until
            .as_deref()
            .map(str::trim)
            .filter(|until| !until.is_empty())
        {
            Some(until) => spoken(&format!("{text}\n\n{UNTIL}{until}"))?,
            None => text,
        };
        let sources = self.sources(&record).await?;
        let mut corrections = record.corrections.clone();
        if let Some(words) = &correction
            && !corrections.contains(words)
        {
            push_limited(&mut corrections, words.clone(), NOTES);
        }
        let (builds, serves) = {
            let mut grounds: Vec<&str> = record.earlier.iter().map(String::as_str).collect();
            grounds.extend(record.report.as_ref().map(|report| report.text.as_str()));
            grounds.push(&record.request);
            grounds.extend(corrections.iter().map(String::as_str));
            grounds.extend(record.notes.iter().map(String::as_str));
            let builds = sources.builds_on(&say.builds_on, &grounds).ok_or_else(|| {
                anyhow::anyhow!(
                    "builds_on 要照抄一段原话，表明这句话接着什么说；没有找到「{}」。找过：这一路最近的几条结论、用户的原话和纠正、用户在会话里说的话、用户对你说过的话、长期规矩、别的路的结论。{}",
                    truncate_chars(say.builds_on.trim(), 60),
                    conclusion_opening(&record)
                )
            })?;
            let anchor = match record.mode {
                DriveMode::Anchored => Anchor::read(&record.project_home).unwrap_or_default(),
                DriveMode::Simple => Anchor::default(),
            };
            let mut purposes: Vec<&str> =
                vec![record.request.as_str(), anchor.deliverable.as_str()];
            purposes.extend(corrections.iter().map(String::as_str));
            purposes.extend(
                anchor
                    .acceptance
                    .iter()
                    .map(|(_, statement, _)| statement.as_str()),
            );
            purposes.extend(anchor.corrections.iter().map(|(_, text, _)| text.as_str()));
            let serves = sources.serves(&say.serves, &purposes).ok_or_else(|| {
                anyhow::anyhow!(
                    "serves 要照抄用户原话（或纠正、验收清单、用户对你说过的话、长期规矩）里的一段，表明这一步为了源头的哪一点；没有找到「{}」。这一路的原话开头是「{}」",
                    truncate_chars(say.serves.trim(), 60),
                    truncate_chars(record.request.trim(), 60)
                )
            })?;
            (builds, serves)
        };
        // Words the person told the Router that a step quotes become part of the goal.
        for found in [&builds, &serves] {
            if let Found::Heard(words) = found
                && !corrections.contains(words)
            {
                push_limited(&mut corrections, words.clone(), NOTES);
            }
        }
        let from = match builds {
            Found::Lane(lane) => Some(lane),
            Found::Own | Found::Heard(_) => None,
        };
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
            from,
        };
        let changed = self
            .change_locked(thread_id, |record| {
                record.acted = Some(now_iso());
                record.max_steps = max_steps;
                record.refused = None;
                record.wait = None;
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
