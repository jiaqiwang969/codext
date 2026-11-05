//! The worker's tools (prototype `src/app/tools/executor.ts`): `superagent_ack`,
//! `superagent_report` and `superagent_handback`, exposed only on a ticket's worker thread.

use std::collections::BTreeMap;
use std::sync::Arc;

use codex_extension_api::ExtensionData;
use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolExecutorFuture;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolPayload;
use codex_extension_api::ToolSpec;
use codex_tools::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::core::ReportState;
use crate::executor::CodextThreadExecutor;
use crate::executor::WorkerReport;

pub const ACK_TOOL: &str = "superagent_ack";
pub const REPORT_TOOL: &str = "superagent_report";
pub const HANDBACK_TOOL: &str = "superagent_handback";

/// Contributes the worker tools to the threads the executor runs tickets on.
pub struct WorkerTools {
    executor: Arc<dyn Fn() -> Option<CodextThreadExecutor> + Send + Sync>,
}

impl WorkerTools {
    pub fn new(executor: Arc<dyn Fn() -> Option<CodextThreadExecutor> + Send + Sync>) -> Self {
        Self { executor }
    }
}

impl ToolContributor for WorkerTools {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>> {
        let Some(executor) = (self.executor)() else {
            return Vec::new();
        };
        let thread_id = thread_store.level_id().to_string();
        if executor.ticket_of_thread(&thread_id).is_none() {
            return Vec::new();
        }
        [Kind::Ack, Kind::Report, Kind::Handback]
            .into_iter()
            .map(|kind| {
                Arc::new(WorkerTool {
                    kind,
                    executor: executor.clone(),
                    thread_id: thread_id.clone(),
                }) as Arc<dyn for<'call> ToolExecutor<ToolCall<'call>>>
            })
            .collect()
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Ack,
    Report,
    Handback,
}

struct WorkerTool {
    kind: Kind,
    executor: CodextThreadExecutor,
    thread_id: String,
}

#[derive(Deserialize)]
struct AckArgs {
    trigger_id: String,
    text: String,
}

#[derive(Deserialize)]
struct ReportArgs {
    state: ReportState,
    summary: String,
    #[serde(default)]
    changes: Vec<String>,
    #[serde(default)]
    tests_run: Vec<String>,
    #[serde(default)]
    risks: Vec<String>,
    #[serde(default)]
    evidence: Vec<String>,
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
    #[serde(default)]
    answers: Vec<String>,
}

#[derive(Deserialize)]
struct HandbackArgs {
    trigger_id: String,
    reason: String,
}

fn strings(description: &str) -> JsonSchema {
    JsonSchema::array(JsonSchema::string(None), Some(description.to_owned()))
}

pub fn ack_spec() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: ACK_TOOL.to_owned(),
        description: "Acknowledge one original human request before substantial work: one sentence, at most 100 words, saying what you will change and into what. It posts as a reply to that message.".to_owned(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            BTreeMap::from([
                ("trigger_id".to_owned(), JsonSchema::string(Some("The person's message you acknowledge.".to_owned()))),
                ("text".to_owned(), JsonSchema::string(Some("One sentence, at most 100 words.".to_owned()))),
            ]),
            Some(vec!["trigger_id".to_owned(), "text".to_owned()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub fn report_spec() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: REPORT_TOOL.to_owned(),
        description: "Report progress, ready, blocked, failed or dropped in the five-line format: changes, tests_run, risks, evidence (paths under the ticket's evidence directory) and the summary (at most 600 characters). ready needs evidence and no helper still running. A question for a person goes in question with reply_to; it cannot share a report with ready.".to_owned(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            BTreeMap::from([
                (
                    "state".to_owned(),
                    JsonSchema::string_enum(
                        ["progress", "waiting", "ready", "blocked", "failed", "dropped"].into_iter().map(serde_json::Value::from).collect(),
                        Some("What the report says about the work.".to_owned()),
                    ),
                ),
                ("summary".to_owned(), JsonSchema::string(Some("The conclusion, at most 600 characters.".to_owned()))),
                ("changes".to_owned(), strings("What changed, one line each.")),
                ("tests_run".to_owned(), strings("The commands run and their results.")),
                ("risks".to_owned(), strings("What could still be wrong.")),
                ("evidence".to_owned(), strings("Paths under ~/org/pm-codext/evidence/<ticket>/.")),
                ("question".to_owned(), JsonSchema::string(Some("A question a person must answer before the work can go on.".to_owned()))),
                ("reply_to".to_owned(), JsonSchema::string(Some("The person's message the question replies to.".to_owned()))),
                ("answers".to_owned(), strings("The people's questions this report answers, by message id.")),
            ]),
            Some(vec!["state".to_owned(), "summary".to_owned()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub fn handback_spec() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: HANDBACK_TOOL.to_owned(),
        description: "Hand back a message that asks for work beyond this ticket or for another one, so the PM assesses it afresh; once per message. queued is false when another ticket still decides it.".to_owned(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            BTreeMap::from([
                ("trigger_id".to_owned(), JsonSchema::string(Some("The message to hand back.".to_owned()))),
                ("reason".to_owned(), JsonSchema::string(Some("Why it is beyond this ticket, in one sentence.".to_owned()))),
            ]),
            Some(vec!["trigger_id".to_owned(), "reason".to_owned()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

fn arguments(payload: &ToolPayload) -> String {
    match payload {
        ToolPayload::Function { arguments } => arguments.clone(),
        ToolPayload::Custom { input } => input.clone(),
        ToolPayload::ToolSearch { .. } => "{}".to_owned(),
    }
}

fn parse<T: serde::de::DeserializeOwned>(arguments: &str) -> Result<T, FunctionCallError> {
    serde_json::from_str(arguments)
        .map_err(|err| FunctionCallError::RespondToModel(format!("参数不对：{err}")))
}

impl<'call> ToolExecutor<ToolCall<'call>> for WorkerTool {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(match self.kind {
            Kind::Ack => ACK_TOOL,
            Kind::Report => REPORT_TOOL,
            Kind::Handback => HANDBACK_TOOL,
        })
    }

    fn spec(&self) -> ToolSpec {
        match self.kind {
            Kind::Ack => ack_spec(),
            Kind::Report => report_spec(),
            Kind::Handback => handback_spec(),
        }
    }

    fn handle<'a>(&'a self, invocation: ToolCall<'call>) -> ToolExecutorFuture<'a>
    where
        ToolCall<'call>: 'a,
    {
        let arguments = arguments(&invocation.payload);
        Box::pin(async move {
            let value = match self.kind {
                Kind::Ack => {
                    let args: AckArgs = parse(&arguments)?;
                    self.executor
                        .ack_from_worker(&self.thread_id, &args.trigger_id, &args.text)
                        .await
                        .map_err(FunctionCallError::RespondToModel)?;
                    json!({ "acknowledged": args.trigger_id })
                }
                Kind::Report => {
                    let args: ReportArgs = parse(&arguments)?;
                    if args.state == ReportState::Ready {
                        if args.evidence.is_empty() {
                            return Err(FunctionCallError::RespondToModel(
                                "ready 需要证据：先把证据写到 ~/org/pm-codext/evidence/<工单名>/ 下，再在 evidence 里给出路径。"
                                    .to_owned(),
                            ));
                        }
                        let missing: Vec<&String> = args
                            .evidence
                            .iter()
                            .filter(|p| !expand_home(p).exists())
                            .collect();
                        if !missing.is_empty() {
                            return Err(FunctionCallError::RespondToModel(format!(
                                "evidence 里这些路径不存在：{missing:?}。先写文件再报 ready。"
                            )));
                        }
                    }
                    let report = WorkerReport {
                        state: args.state,
                        summary: args.summary,
                        changes: args.changes,
                        tests_run: args.tests_run,
                        risks: args.risks,
                        evidence: args.evidence,
                        question: args.question,
                        reply_to: args.reply_to,
                        answers: args.answers,
                    };
                    self.executor
                        .report_from_worker(&self.thread_id, report)
                        .await
                        .map_err(FunctionCallError::RespondToModel)?;
                    json!({ "recorded": true })
                }
                Kind::Handback => {
                    let args: HandbackArgs = parse(&arguments)?;
                    let queued = self
                        .executor
                        .handback_from_worker(&self.thread_id, &args.trigger_id, &args.reason)
                        .await
                        .map_err(FunctionCallError::RespondToModel)?;
                    json!({ "queued": queued })
                }
            };
            Ok(Box::new(JsonToolOutput::new(value)) as Box<dyn ToolOutput>)
        })
    }
}

/// `~/x` as the org user's home; other paths as given.
fn expand_home(path: &str) -> std::path::PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(|home| std::path::PathBuf::from(home).join(rest))
            .unwrap_or_else(|| std::path::PathBuf::from(path)),
        None => std::path::PathBuf::from(path),
    }
}
