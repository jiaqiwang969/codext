//! The person's standing rules in the Router's context (`drive/standing.rs`): told whole when they
//! change or the history lost them, so a compacted Router still has them word for word.

use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::WorldStateSectionContribution;
use codex_extension_api::input_text;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

const STANDING_ID: &str = "superagent_standing";
const STANDING_MARKERS: (&str, &str) = ("<superagent_standing>", "</superagent_standing>");

/// The section; `lines` are the rules' lines, oldest first. The snapshot is a digest of them.
pub fn standing_section(lines: Vec<String>) -> WorldStateSectionContribution {
    let digest = Sha256::digest(lines.join("\n").as_bytes());
    let snapshot = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    WorldStateSectionContribution::new(STANDING_ID, move |previous| {
        let fragment = match previous {
            PreviousWorldStateSection::Known(Value::String(told)) if *told == snapshot => None,
            PreviousWorldStateSection::Known(_)
            | PreviousWorldStateSection::Unknown
            | PreviousWorldStateSection::Absent => {
                let mut text = String::from(
                    "用户的长期规矩（原话，每条带范围；引用时照抄，可作 builds_on 和 serves 的依据；用户改口时 forget 旧的再 remember 新的）：\n",
                );
                if lines.is_empty() {
                    text.push_str("（还没有）\n");
                }
                for line in &lines {
                    text.push_str(line);
                    text.push('\n');
                }
                Some(RenderedWorldStateFragment::new(
                    "developer",
                    STANDING_MARKERS,
                    input_text(text),
                ))
            }
        };
        (Some(Value::String(snapshot.clone())), fragment)
    })
    .with_retained_fragment_matcher(|role, text| {
        role == "developer" && text.trim_start().starts_with(STANDING_MARKERS.0)
    })
}
