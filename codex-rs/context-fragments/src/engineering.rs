//! Bounded, attributed context supplied by an engineering Project adapter.

use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::input_text;
use codex_utils_string::approx_bytes_for_tokens;
use std::io;

const ENGINEERING_CONTEXT_START: &str = "<engineering_project_context>";
const ENGINEERING_CONTEXT_END: &str = "</engineering_project_context>";
const MAX_ENGINEERING_CONTEXT_TOKENS: usize = 4_000;

/// A bounded Project/workspace snapshot rendered as developer context.
///
/// The value is expected to be an already validated contract payload. This
/// type does not parse, authorize, or mutate Project data; those operations
/// remain owned by the ontology adapter and Rust runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineeringProjectContext {
    value: String,
}

impl EngineeringProjectContext {
    /// Rejects oversized context so a JSON contract is never silently truncated.
    pub fn try_new(value: impl Into<String>) -> io::Result<Self> {
        let value = value.into();
        if value.len() > approx_bytes_for_tokens(MAX_ENGINEERING_CONTEXT_TOKENS) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "engineering Project context exceeds {MAX_ENGINEERING_CONTEXT_TOKENS} estimated tokens"
                ),
            ));
        }
        Ok(Self { value })
    }

    /// Returns the validated payload without the rendering markers.
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl super::ContextualUserFragment for EngineeringProjectContext {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("engineering.project_context".to_string())
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (ENGINEERING_CONTEXT_START, ENGINEERING_CONTEXT_END)
    }

    fn body(&self) -> ContentItem {
        input_text(format!("\n{}\n", self.value))
    }
}
