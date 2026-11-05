//! The Router's list of driven sessions: one line each (session, goal, steps, state), told whole
//! once and then only the lines that changed and the sessions released.

use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::WorldStateSectionContribution;
use codex_extension_api::input_text;
use serde_json::Value;

use crate::drive::DriveRecord;
use crate::drive::DriveState;

const DRIVES_ID: &str = "superagent_drives";
const DRIVES_MARKERS: (&str, &str) = ("<superagent_drives>", "</superagent_drives>");
const DRIVES_UPDATE_MARKERS: (&str, &str) =
    ("<superagent_drives_update>", "</superagent_drives_update>");

/// What a driven session waits for from the person, by its state.
pub fn drive_attention(record: &DriveRecord) -> Option<&'static str> {
    match record.state {
        DriveState::Driving => None,
        DriveState::Reported { .. } => Some("交了结论，等你转告用户或回它"),
        DriveState::Deliverable { .. } => Some("可交付，等你验收"),
        DriveState::NeedsDecision { .. } => Some("推进需要你决定"),
        DriveState::Paused { .. } => Some("推进已暂停"),
    }
}

/// The section; `lines` are (session id, line) in key order.
pub fn drives_section(lines: Vec<(String, String)>) -> WorldStateSectionContribution {
    WorldStateSectionContribution::new(DRIVES_ID, move |previous| {
        let snapshot: serde_json::Map<String, Value> = lines
            .iter()
            .map(|(id, line)| (id.clone(), Value::String(line.clone())))
            .collect();
        let fragment = match previous {
            PreviousWorldStateSection::Known(Value::Object(before)) => {
                let mut text = String::new();
                for (id, line) in &lines {
                    if before.get(id).and_then(Value::as_str) != Some(line.as_str()) {
                        text.push_str(line);
                        text.push('\n');
                    }
                }
                let released: Vec<&str> = before
                    .keys()
                    .map(String::as_str)
                    .filter(|id| !lines.iter().any(|(line_id, _)| line_id == id))
                    .collect();
                if !released.is_empty() {
                    text.push_str(&format!("已不再推进：{}\n", released.join("、")));
                }
                (!text.is_empty()).then(|| {
                    RenderedWorldStateFragment::new(
                        "developer",
                        DRIVES_UPDATE_MARKERS,
                        input_text(format!(
                            "推进中的会话有变化（列同 <superagent_drives>）：\n{text}"
                        )),
                    )
                })
            }
            PreviousWorldStateSection::Known(_)
            | PreviousWorldStateSection::Unknown
            | PreviousWorldStateSection::Absent => (!lines.is_empty()).then(|| {
                let mut text = String::from("由引擎推进的会话。列：会话 | 目标 | 步数 | 状态\n");
                for (_, line) in &lines {
                    text.push_str(line);
                    text.push('\n');
                }
                RenderedWorldStateFragment::new("developer", DRIVES_MARKERS, input_text(text))
            }),
        };
        (Some(Value::Object(snapshot)), fragment)
    })
    .with_retained_fragment_matcher(|role, text| {
        role == "developer" && text.trim_start().starts_with(DRIVES_MARKERS.0)
    })
}
