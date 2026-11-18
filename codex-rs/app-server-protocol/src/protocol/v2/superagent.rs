//! Superagent coordination engine (`codex-rs/ext/superagent`): the status its external watchdog
//! and the Mac launcher read.

use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SuperagentStatusResponse {
    /// 0 normal; 2 a part is in error or a ticket is stalled; 3 waiting for configuration.
    pub exit_code: i32,
    /// The engine's status: health by part, effect counts, open tickets, questions waiting for a
    /// person, the hooks' error count and the watchdog's heartbeat.
    pub status: serde_json::Value,
}
