//! App-server API types for the account profile a thread runs on.

use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

/// A thread now runs on another account-pool profile. Sent after `account/profile/switch`
/// rebinds a loaded thread, to every connection subscribed to it except the one that asked for
/// the switch; that connection learns it from the response.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadAccountProfileUpdatedNotification {
    pub thread_id: String,
    /// The profile's directory name under `CODEX_HOME/accounts`, as `thread.extra.accountProfile`
    /// reports it.
    pub account_profile: String,
}
