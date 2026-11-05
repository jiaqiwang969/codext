//! `superagent_drive`: the Router takes a session under drive toward an anchored goal, pauses it
//! when the person takes over, resumes it when they hand it back, stops it, or reads the steps
//! the engine sent. Offered only while the Router drives (`router-mode` = `drive`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

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
use serde_json::json;

use super::json_output;
use super::parse_arguments;
use crate::cards::truncate_chars;
use crate::drive::DriveRecord;
use crate::drive::DriveService;

pub const DRIVE_TOOL: &str = "superagent_drive";
const PROMPT_CHARS: usize = 1_500;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Action {
    Start,
    Pause,
    Resume,
    Stop,
    History,
}

#[derive(Debug, Deserialize)]
struct DriveArgs {
    action: Action,
    thread_id: String,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    max_steps: Option<u32>,
}

type Lookup = Arc<dyn Fn() -> Option<DriveService> + Send + Sync>;

pub struct DriveTool {
    drive: Lookup,
}

impl DriveTool {
    pub fn new(drive: Lookup) -> Self {
        Self { drive }
    }

    fn spec() -> ToolSpec {
        let properties = BTreeMap::from([
            (
                "action".to_owned(),
                JsonSchema::string_enum(
                    vec![json!("start"), json!("pause"), json!("resume"), json!("stop"), json!("history")],
                    Some("start: drive a session toward the goal anchored in its Project; pause: the person takes the session over; resume: hand it back, the next judgment reads the Project and the person's notes again; stop: release it; history: the steps sent so far with their judgments.".to_owned()),
                ),
            ),
            ("thread_id".to_owned(), JsonSchema::string(Some("The session.".to_owned()))),
            (
                "project".to_owned(),
                JsonSchema::string(Some("start: the project directory, or its .engineering workspace; it must have a committed route anchor.".to_owned())),
            ),
            (
                "title".to_owned(),
                JsonSchema::string(Some("start: a short name of the goal, in the person's words.".to_owned())),
            ),
            (
                "max_steps".to_owned(),
                JsonSchema::integer(Some("start: the most steps to send before pausing; 20 by default.".to_owned())),
            ),
        ]);
        ToolSpec::Function(ResponsesApiTool {
            name: DRIVE_TOOL.to_owned(),
            description: "Drive a session toward its anchored goal: after each of its turns the engine runs route-audit and sends the next step, or stops when the goal is deliverable, drifts, or needs the person.".to_owned(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["action".to_owned(), "thread_id".to_owned()]),
                Some(false.into()),
            ),
            output_schema: None,
        })
    }

    async fn run(&self, arguments: &str) -> Result<Value, FunctionCallError> {
        let args: DriveArgs = parse_arguments(arguments)?;
        let Some(drive) = (self.drive)() else {
            return Err(FunctionCallError::RespondToModel(
                "推进服务还没有启动".to_owned(),
            ));
        };
        let refused = |err: anyhow::Error| FunctionCallError::RespondToModel(format!("{err:#}"));
        let missing =
            || FunctionCallError::RespondToModel(format!("会话 {} 不在推进记录里", args.thread_id));
        let answer =
            |record: DriveRecord| json!({ "thread_id": record.thread_id, "drive": record.line() });
        match args.action {
            Action::Start => {
                let project = args.project.as_deref().map(str::trim).unwrap_or_default();
                if project.is_empty() {
                    return Err(FunctionCallError::RespondToModel(
                        "start 需要 project：项目目录或它的 .engineering".to_owned(),
                    ));
                }
                let project = PathBuf::from(project);
                let home = if project.join(".engineering").is_dir() {
                    project.join(".engineering")
                } else {
                    project
                };
                let title = args
                    .title
                    .as_deref()
                    .map(str::trim)
                    .filter(|title| !title.is_empty())
                    .unwrap_or("目标")
                    .to_owned();
                drive
                    .start(&args.thread_id, home, title, args.max_steps)
                    .await
                    .map(answer)
                    .map_err(refused)
            }
            Action::Pause => drive
                .pause(&args.thread_id, "你接管了这个会话")
                .await
                .map_err(refused)?
                .map(answer)
                .ok_or_else(missing),
            Action::Resume => drive
                .resume(&args.thread_id)
                .await
                .map_err(refused)?
                .map(answer)
                .ok_or_else(missing),
            Action::Stop => drive
                .stop(&args.thread_id)
                .await
                .map_err(refused)?
                .map(|record| json!({ "thread_id": record.thread_id, "drive": "已释放" }))
                .ok_or_else(missing),
            Action::History => {
                let record = drive
                    .record(&args.thread_id)
                    .await
                    .map_err(refused)?
                    .ok_or_else(missing)?;
                let steps: Vec<Value> = record
                    .history
                    .iter()
                    .map(|step| {
                        json!({
                            "n": step.n,
                            "at": step.at,
                            "audit": step.audit,
                            "focus": step.focus,
                            "prompt": truncate_chars(&step.prompt, PROMPT_CHARS),
                        })
                    })
                    .collect();
                Ok(json!({ "drive": record.line(), "steps": steps }))
            }
        }
    }
}

impl<'call> ToolExecutor<ToolCall<'call>> for DriveTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(DRIVE_TOOL)
    }

    fn spec(&self) -> ToolSpec {
        Self::spec()
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::DirectModelOnly
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        false
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
