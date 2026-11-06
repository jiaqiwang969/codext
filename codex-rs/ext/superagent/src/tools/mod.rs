//! The native tools the engine contributes to codext threads: the PM's routing tools, the
//! workers' report tools and the `superagent_read` tool both roles share (prototype `src/app/tools/`).

pub mod drive;
pub mod pm;
pub mod read;
pub mod worker;

use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ToolOutput;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// The most words a message to a person may have (prototype `infra/text.ts`).
pub const IM_WORDS: usize = 100;
/// The most words a routing note may have.
pub const NOTE_WORDS: usize = 30;

/// Counts word-like units across Chinese, English and mixed text: each CJK character is a word, a
/// run of letters or digits is one word, punctuation and spaces count for nothing. It approximates
/// the prototype's dictionary segmentation from above, so a text it accepts is within the limit there too.
pub fn word_count(text: &str) -> usize {
    let mut count = 0;
    let mut in_run = false;
    for c in text.chars() {
        let cjk = matches!(c as u32,
            0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x3040..=0x30FF | 0xAC00..=0xD7AF | 0x20000..=0x2FA1F);
        if cjk {
            count += 1;
            in_run = false;
        } else if c.is_alphanumeric() || c == '_' || c == '\'' {
            if !in_run {
                count += 1;
                in_run = true;
            }
        } else {
            in_run = false;
        }
    }
    count
}

/// Parses a tool call's JSON arguments, answering the model with the parse error.
pub fn parse_arguments<T: DeserializeOwned>(arguments: &str) -> Result<T, FunctionCallError> {
    serde_json::from_str(arguments)
        .map_err(|err| FunctionCallError::RespondToModel(format!("invalid arguments: {err}")))
}

pub fn json_output(value: Value) -> Box<dyn ToolOutput> {
    Box::new(JsonToolOutput::new(value))
}

