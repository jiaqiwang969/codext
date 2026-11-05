//! `superagent_read`: one page of a resource, for the PM or for a worker reading its own ticket
//! (prototype `src/app/tools/tool.ts` `readTool`).

use std::collections::BTreeMap;

use codex_extension_api::FunctionCallError;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolSpec;
use codex_tools::JsonSchema;
use codex_tools::ToolExposure;
use serde::Deserialize;
use serde_json::Value;

use super::json_output;
use super::parse_arguments;
use crate::cards::SUMMARY_CHARS;
use crate::cards::preview;
use crate::cards::truncate_chars;
use crate::host::SuperagentHost;
use crate::resources::LIST_DEFAULT;
use crate::resources::LIST_MAX;
use crate::resources::ReadError;
use crate::resources::ReadScope;
use crate::resources::TEXT_DEFAULT;
use crate::resources::TEXT_MAX;
use crate::resources::read_resource;
use crate::resources::resource_grammar;
use crate::store::Store;
use codex_protocol::ThreadId;
use serde_json::json;
use std::sync::Arc;

pub const READ_TOOL: &str = "superagent_read";

#[derive(Debug, Deserialize)]
pub struct ReadArgs {
    pub resource: String,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// The turns `session/<id>` shows by default and at most.
const SESSION_TURNS_DEFAULT: usize = 3;
const SESSION_TURNS_MAX: usize = 10;

pub struct ReadTool {
    store: Store,
    scope: ReadScope,
    /// Where the Router reads the host's sessions; workers have none.
    sessions: Option<Arc<dyn SuperagentHost>>,
}

impl ReadTool {
    pub fn new(store: Store, scope: ReadScope) -> Self {
        Self {
            store,
            scope,
            sessions: None,
        }
    }

    /// The Router's read tool: the ledger's resources and the host's sessions, read-only.
    pub fn for_pm(store: Store, host: Arc<dyn SuperagentHost>) -> Self {
        Self {
            store,
            scope: ReadScope::Pm,
            sessions: Some(host),
        }
    }

    /// `sessions[/<cursor>]` and `session/<thread id>`: the host's sessions, read without loading
    /// or resuming any of them. `None` when the resource is not a session resource.
    async fn read_sessions(&self, args: &ReadArgs) -> Option<Result<Value, FunctionCallError>> {
        let host = self.sessions.as_ref()?;
        let failed = |err: anyhow::Error| FunctionCallError::RespondToModel(format!("{err:#}"));
        if args.resource == "sessions" || args.resource.starts_with("sessions/") {
            let cursor = args.resource.strip_prefix("sessions/").map(str::to_owned);
            let limit = args.limit.unwrap_or(LIST_DEFAULT).clamp(1, LIST_MAX) as u32;
            return Some(match host.list_sessions(cursor, limit).await {
                Ok(page) => {
                    let items: Vec<Value> = page
                        .sessions
                        .into_iter()
                        .map(|session| {
                            json!({
                                "id": session.id,
                                "name": session.name,
                                "preview": preview(&session.preview),
                                "cwd": session.cwd,
                                "status": session.status,
                                "updatedAt": session.updated_at,
                                "account": session.account_profile,
                            })
                        })
                        .collect();
                    Ok(json!({
                        "items": items,
                        "next_resource": page.next_cursor.map(|cursor| format!("sessions/{cursor}")),
                    }))
                }
                Err(err) => Err(failed(err)),
            });
        }
        let id = args.resource.strip_prefix("session/")?;
        let thread = match ThreadId::from_string(id) {
            Ok(thread) => thread,
            Err(err) => {
                return Some(Err(FunctionCallError::RespondToModel(format!(
                    "{id} is not a session id: {err}"
                ))));
            }
        };
        let limit = args
            .limit
            .unwrap_or(SESSION_TURNS_DEFAULT)
            .clamp(1, SESSION_TURNS_MAX) as u32;
        let clip = |texts: Vec<String>| -> Vec<String> {
            texts
                .iter()
                .map(|text| truncate_chars(text, SUMMARY_CHARS))
                .collect()
        };
        Some(match host.session_turns(thread, limit).await {
            Ok(turns) => {
                let turns: Vec<Value> = turns
                    .into_iter()
                    .map(|turn| {
                        json!({
                            "id": turn.id,
                            "status": turn.status,
                            "user": clip(turn.user),
                            "assistant": clip(turn.assistant),
                        })
                    })
                    .collect();
                Ok(json!({ "session": id, "turns": turns }))
            }
            Err(err) => Err(failed(err)),
        })
    }

    pub fn spec_for(scope: &ReadScope) -> ToolSpec {
        let worker = matches!(scope, ReadScope::Worker { .. });
        let properties = BTreeMap::from([
            (
                "resource".to_owned(),
                JsonSchema::string(Some(format!("The resource, one of: {}.", resource_grammar(worker)))),
            ),
            (
                "offset".to_owned(),
                JsonSchema::integer(Some("Copy next_offset from the previous page; it counts characters of a text and items of a list. 0 by default.".to_owned())),
            ),
            (
                "limit".to_owned(),
                JsonSchema::integer(Some(format!("Characters of a text, {TEXT_DEFAULT} by default and {TEXT_MAX} at most, or items of a list, {LIST_DEFAULT} by default and {LIST_MAX} at most."))),
            ),
        ]);
        ToolSpec::Function(ResponsesApiTool {
            name: READ_TOOL.to_owned(),
            description: if worker {
                "Read one page of a resource of your own ticket, such as the ticket or an input. Follow next_offset, and next_resource where a page names one.".to_owned()
            } else {
                "Read one page of a resource the resource parameter lists, such as the ticket list or a ticket. Follow next_offset, and next_resource where a page names one.".to_owned()
            },
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["resource".to_owned()]),
                Some(false.into()),
            ),
            output_schema: None,
        })
    }

    pub async fn run(&self, arguments: &str) -> Result<Value, FunctionCallError> {
        let args: ReadArgs = parse_arguments(arguments)?;
        if let Some(result) = self.read_sessions(&args).await {
            return result;
        }
        read_resource(
            &self.store,
            &self.scope,
            &args.resource,
            args.offset.unwrap_or(0),
            args.limit,
        )
        .await
        .map_err(|err| match err {
            ReadError::Refused(text) => FunctionCallError::RespondToModel(text),
            ReadError::User(user) => FunctionCallError::RespondToModel(user.message),
            ReadError::Other(other) => FunctionCallError::Fatal(format!("{other:#}")),
        })
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for ReadTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(READ_TOOL)
    }

    fn spec(&self) -> ToolSpec {
        Self::spec_for(&self.scope)
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::DirectModelOnly
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, invocation: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        'call: 'a,
    {
        Box::pin(async move {
            let arguments = invocation.function_arguments()?.to_owned();
            let value = self.run(&arguments).await?;
            Ok(json_output(value))
        })
    }
}
