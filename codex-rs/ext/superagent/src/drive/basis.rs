//! Where the words a step quotes come from (`say.rs`). A step follows from something said
//! (`builds_on`) and serves something the person asked for (`serves`); both must be copied from
//! what really was said, so a Router that misremembers cannot send a step detached from what
//! happened. Besides the session's own records (its latest conclusions, the person's request and
//! corrections, what the person typed into it), the words may come from what the person told the
//! Router (then they join the session's corrections), from the person's standing rules
//! (`standing.rs`), and, for `builds_on`, from another session's conclusions (the step then says
//! which session it quotes).

use super::DriveRecord;
use super::DriveService;
use super::say::found_in;
use crate::cards::truncate_chars;

/// Where a quote was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Found {
    /// In the session's own records.
    Own,
    /// In what the person told the Router: these words, which join the session's corrections.
    Heard(String),
    /// In the conclusions of another session, `title (thread)`.
    Lane(String),
}

/// The words a quote may come from beyond the session's own records.
#[derive(Clone, Debug, Default)]
pub(super) struct Sources {
    /// What the person told the Router lately, oldest first.
    pub(super) heard: Vec<String>,
    /// The person's standing rules that hold for the session.
    pub(super) standing: Vec<String>,
    /// The other sessions under drive: `title (thread)` and their latest conclusions.
    pub(super) lanes: Vec<(String, Vec<String>)>,
}

impl Sources {
    /// Where `builds_on` was found, the session's own `grounds` first; `None` when nowhere.
    pub(super) fn builds_on(&self, quote: &str, grounds: &[&str]) -> Option<Found> {
        if found_in(quote, grounds.iter().copied())
            || found_in(quote, self.standing.iter().map(String::as_str))
        {
            return Some(Found::Own);
        }
        if let Some(words) = self
            .heard
            .iter()
            .rev()
            .find(|words| found_in(quote, [words.as_str()]))
        {
            return Some(Found::Heard(words.clone()));
        }
        self.lanes
            .iter()
            .find(|(_, texts)| found_in(quote, texts.iter().map(String::as_str)))
            .map(|(lane, _)| Found::Lane(lane.clone()))
    }

    /// Where `serves` was found, the session's own `purposes` first; another session's words do
    /// not say what this one is for.
    pub(super) fn serves(&self, quote: &str, purposes: &[&str]) -> Option<Found> {
        if found_in(quote, purposes.iter().copied())
            || found_in(quote, self.standing.iter().map(String::as_str))
        {
            return Some(Found::Own);
        }
        self.heard
            .iter()
            .rev()
            .find(|words| found_in(quote, [words.as_str()]))
            .map(|words| Found::Heard(words.clone()))
    }
}

/// The opening words of the session's latest conclusion, for a refusal's hint.
pub(super) fn conclusion_opening(record: &DriveRecord) -> String {
    record
        .report
        .as_ref()
        .map(|report| {
            let flat = report.text.split_whitespace().collect::<Vec<_>>().join(" ");
            format!("这一路最近的结论开头是「{}」", truncate_chars(&flat, 80))
        })
        .unwrap_or_else(|| "这一路还没有结论".to_owned())
}

impl DriveService {
    /// What a step for `record` may quote beyond its own records.
    pub(super) async fn sources(&self, record: &DriveRecord) -> anyhow::Result<Sources> {
        let lanes = self
            .records()
            .await?
            .into_iter()
            .filter(|other| other.thread_id != record.thread_id)
            .map(|other| {
                let mut texts = other.earlier.clone();
                texts.extend(other.report.as_ref().map(|report| report.text.clone()));
                (format!("{}（{}）", other.title, other.thread_id), texts)
            })
            .collect();
        Ok(Sources {
            heard: self.heard_words().await?,
            standing: self.standing_for(record).await?,
            lanes,
        })
    }
}
