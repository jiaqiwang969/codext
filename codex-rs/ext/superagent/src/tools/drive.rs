//! `superagent_drive`: the Router keeps a session moving on the person's behalf. It starts a
//! drive with the person's words and its own first message (opening a folder and a session when
//! there is none), answers each conclusion the worker reports with `say` (the engine checks the
//! basis), passes on acceptance feedback, pauses, hands back, stops, or reads what was said.
//! Offered while the Router drives (`router-mode` = `drive` or `org`).

use std::collections::BTreeMap;
use std::path::Path;
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
use crate::drive::DriveMode;
use crate::drive::DriveRecord;
use crate::drive::DriveService;
use crate::drive::DriveStart;
use crate::drive::Feedback;
use crate::drive::Goal;
use crate::drive::Say;

pub const DRIVE_TOOL: &str = "superagent_drive";
const PROMPT_CHARS: usize = 1_500;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Action {
    Start,
    Say,
    Interrupt,
    #[serde(rename = "switch_account")]
    SwitchAccount,
    Pause,
    Resume,
    Stop,
    History,
    Feedback,
}

#[derive(Debug, Deserialize)]
struct DriveArgs {
    action: Action,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default)]
    builds_on: Option<String>,
    #[serde(default)]
    serves: Option<String>,
    #[serde(default)]
    correction: Option<String>,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    max_steps: Option<u32>,
    #[serde(default)]
    request: Option<String>,
    #[serde(default)]
    deliverable: Option<String>,
    #[serde(default)]
    acceptance: Option<Vec<String>>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    passed: Option<Vec<String>>,
    #[serde(default)]
    failed: Option<Vec<String>>,
}

type Lookup = Arc<dyn Fn() -> Option<DriveService> + Send + Sync>;

/// The Project workspace of a project directory (or the workspace itself).
fn workspace_of(project: &Path) -> PathBuf {
    if project
        .file_name()
        .is_some_and(|name| name == ".engineering")
    {
        project.to_owned()
    } else {
        project.join(".engineering")
    }
}

pub struct DriveTool {
    drive: Lookup,
    /// The Router thread that calls; a session it opens runs under the same account.
    caller: Option<String>,
}

impl DriveTool {
    pub fn new(drive: Lookup, caller: Option<String>) -> Self {
        Self { drive, caller }
    }

    fn spec() -> ToolSpec {
        let properties = BTreeMap::from([
            (
                "action".to_owned(),
                JsonSchema::string_enum(
                    vec![json!("start"), json!("say"), json!("interrupt"), json!("switch_account"), json!("feedback"), json!("pause"), json!("resume"), json!("stop"), json!("history")],
                    Some("start: take a session under drive (or open a new session in `folder`) and send it the first message `text`; say: answer the worker's last conclusion with `text`, quoting in `builds_on` and `serves` what it follows from (while the worker's turn runs, the text is said into it instead); interrupt: stop the running step because the person found it going wrong, then resume and say the next thing; switch_account: move a session (driven or not) to another account, `account` or else the next by quota reset time — the engine also does this by itself before a step whose account runs low and when a step fails on quota or model capacity; feedback: the person's acceptance feedback on an anchored goal; pause: the person takes the session over; resume: hand it back; stop: release it; history: the person's words, the last conclusion and every step with its basis.".to_owned()),
                ),
            ),
            ("thread_id".to_owned(), JsonSchema::string(Some("The session; start may leave it out and give `folder` to open a new one.".to_owned()))),
            (
                "account".to_owned(),
                JsonSchema::string(Some("start: the account the session works under from its first step, as `codex-account-next list` names it, when the person named one; leave out to keep the session's account (a new session takes yours). switch_account: the account to move to; leave out to take the next by quota reset time.".to_owned())),
            ),
            (
                "folder".to_owned(),
                JsonSchema::string(Some("start of a simple request: the absolute path of the folder the session works in; created when missing.".to_owned())),
            ),
            (
                "builds_on".to_owned(),
                JsonSchema::string(Some("say: words copied from the worker's last conclusion (or the person's words) this message follows from.".to_owned())),
            ),
            (
                "serves".to_owned(),
                JsonSchema::string(Some("say: words copied from the person's request (or a correction, or an acceptance item) this message serves.".to_owned())),
            ),
            (
                "correction".to_owned(),
                JsonSchema::string(Some("say: what the person just told you about this goal, verbatim; kept with the goal.".to_owned())),
            ),
            (
                "project".to_owned(),
                JsonSchema::string(Some("start of an anchored goal: the project directory, or its .engineering workspace.".to_owned())),
            ),
            (
                "title".to_owned(),
                JsonSchema::string(Some("start: a short name of the goal, in the person's words.".to_owned())),
            ),
            (
                "max_steps".to_owned(),
                JsonSchema::integer(Some("start: the most steps to send before pausing; 20 by default.".to_owned())),
            ),
            (
                "text".to_owned(),
                JsonSchema::string(Some("start, say: the message to the session, in plain words; feedback: the person's words about what they tried, verbatim.".to_owned())),
            ),
            (
                "passed".to_owned(),
                JsonSchema::array(
                    JsonSchema::string(None),
                    Some("feedback: acceptance item ids (A1, ...) the person found met.".to_owned()),
                ),
            ),
            (
                "failed".to_owned(),
                JsonSchema::array(
                    JsonSchema::string(None),
                    Some("feedback: acceptance item ids the person found not met; they are reopened.".to_owned()),
                ),
            ),
            (
                "request".to_owned(),
                JsonSchema::string(Some("start: the person's request, word for word, with their answers to your questions.".to_owned())),
            ),
            (
                "deliverable".to_owned(),
                JsonSchema::string(Some("start, for a project without a route anchor: one sentence naming what is delivered.".to_owned())),
            ),
            (
                "acceptance".to_owned(),
                JsonSchema::array(
                    JsonSchema::string(None),
                    Some("start, for a project without a route anchor: the acceptance items the person confirmed, each one checkable; the engine anchors them before the first step.".to_owned()),
                ),
            ),
        ]);
        ToolSpec::Function(ResponsesApiTool {
            name: DRIVE_TOOL.to_owned(),
            description: "Keep a session moving on the person's behalf: after each of its turns the engine hands you the worker's conclusion (and, for an anchored goal, the route-audit result); you tell the person or answer the worker with say.".to_owned(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                properties,
                Some(vec!["action".to_owned()]),
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
        let given = args
            .thread_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        let thread_id = || {
            given.clone().ok_or_else(|| {
                FunctionCallError::RespondToModel("需要 thread_id：说的是哪个会话".to_owned())
            })
        };
        let missing =
            |id: &str| FunctionCallError::RespondToModel(format!("会话 {id} 不在推进记录里"));
        let answer =
            |record: DriveRecord| json!({ "thread_id": record.thread_id, "drive": record.line() });
        let path = |value: &Option<String>| {
            value
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        match args.action {
            Action::Start => {
                let request = args.request.clone().unwrap_or_default();
                let title = args
                    .title
                    .as_deref()
                    .map(str::trim)
                    .filter(|title| !title.is_empty())
                    .unwrap_or("目标")
                    .to_owned();
                let goal = match (args.deliverable, args.acceptance) {
                    (Some(deliverable), Some(acceptance)) => Some(Goal {
                        request: request.clone(),
                        deliverable,
                        acceptance,
                    }),
                    (None, None) => None,
                    _ => {
                        return Err(FunctionCallError::RespondToModel(
                            "建立锚定需要 deliverable 和 acceptance 一起给".to_owned(),
                        ));
                    }
                };
                let folder = path(&args.folder);
                let (mode, home) = match (path(&args.project), &folder, &goal) {
                    (Some(project), _, _) => (DriveMode::Anchored, workspace_of(&project)),
                    (None, Some(folder), Some(_)) => (DriveMode::Anchored, workspace_of(folder)),
                    (None, Some(folder), None) => (DriveMode::Simple, folder.clone()),
                    (None, None, _) => {
                        return Err(FunctionCallError::RespondToModel(
                            "start 需要 folder（会话在哪个文件夹做）；有锚定的工程项目给 project"
                                .to_owned(),
                        ));
                    }
                };
                let account = args
                    .account
                    .as_deref()
                    .map(str::trim)
                    .filter(|account| !account.is_empty())
                    .map(str::to_owned);
                let thread_id = match given.clone() {
                    Some(id) => id,
                    None => {
                        let folder = folder.clone().ok_or_else(|| {
                            FunctionCallError::RespondToModel(
                                "新开会话需要 folder：在哪个文件夹做".to_owned(),
                            )
                        })?;
                        // A new session runs under the account named, else under the Router's.
                        match account.as_deref() {
                            Some(account) => drive.open_session_on(&folder, account).await,
                            None => drive.open_session(&folder, self.caller.as_deref()).await,
                        }
                        .map_err(refused)?
                    }
                };
                let start = DriveStart {
                    project_home: home,
                    title,
                    mode,
                    max_steps: args.max_steps,
                    request,
                    goal,
                    first: args.text.clone().unwrap_or_default(),
                    account,
                    router: self.caller.clone(),
                };
                drive
                    .start(&thread_id, start)
                    .await
                    .map(answer)
                    .map_err(refused)
            }
            Action::Say => {
                let id = thread_id()?;
                let say = Say {
                    text: args.text.unwrap_or_default(),
                    builds_on: args.builds_on.unwrap_or_default(),
                    serves: args.serves.unwrap_or_default(),
                    correction: args.correction,
                };
                drive
                    .say(&id, say)
                    .await
                    .map_err(refused)?
                    .map(answer)
                    .ok_or_else(|| missing(&id))
            }
            Action::Interrupt => {
                let id = thread_id()?;
                drive
                    .interrupt(&id)
                    .await
                    .map_err(refused)?
                    .map(answer)
                    .ok_or_else(|| missing(&id))
            }
            Action::SwitchAccount => {
                let id = thread_id()?;
                let (record, account) = drive
                    .switch_account(&id, args.account.clone())
                    .await
                    .map_err(refused)?;
                Ok(json!({
                    "thread_id": id,
                    "account": account,
                    "drive": record.map(|record| record.line()),
                }))
            }
            Action::Pause => {
                let id = thread_id()?;
                drive
                    .pause(&id, "你接管了这个会话")
                    .await
                    .map_err(refused)?
                    .map(answer)
                    .ok_or_else(|| missing(&id))
            }
            Action::Resume => {
                let id = thread_id()?;
                if let Some(max_steps) = args.max_steps {
                    drive.set_max_steps(&id, max_steps).await.map_err(refused)?;
                }
                drive
                    .resume(&id)
                    .await
                    .map_err(refused)?
                    .map(answer)
                    .ok_or_else(|| missing(&id))
            }
            Action::Feedback => {
                let id = thread_id()?;
                let feedback = Feedback {
                    text: args.text.unwrap_or_default(),
                    passed: args.passed.unwrap_or_default(),
                    failed: args.failed.unwrap_or_default(),
                };
                drive
                    .feedback(&id, feedback)
                    .await
                    .map_err(refused)?
                    .map(answer)
                    .ok_or_else(|| missing(&id))
            }
            Action::Stop => {
                let id = thread_id()?;
                drive
                    .stop(&id)
                    .await
                    .map_err(refused)?
                    .map(|record| json!({ "thread_id": record.thread_id, "drive": "已释放" }))
                    .ok_or_else(|| missing(&id))
            }
            Action::History => {
                let id = thread_id()?;
                let record = drive
                    .record(&id)
                    .await
                    .map_err(refused)?
                    .ok_or_else(|| missing(&id))?;
                let steps: Vec<Value> = record
                    .history
                    .iter()
                    .map(|step| {
                        json!({
                            "n": step.n,
                            "at": step.at,
                            "text": truncate_chars(&step.text, PROMPT_CHARS),
                            "builds_on": step.builds_on,
                            "serves": step.serves,
                            "audit": step.audit,
                        })
                    })
                    .collect();
                Ok(json!({
                    "drive": record.line(),
                    "request": record.request,
                    "corrections": record.corrections,
                    "notes": record.notes,
                    "report": record.report.as_ref().map(|report| json!({
                        "turn": report.turn,
                        "text": truncate_chars(&report.text, PROMPT_CHARS),
                        "audit": report.audit,
                    })),
                    "steps": steps,
                }))
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
