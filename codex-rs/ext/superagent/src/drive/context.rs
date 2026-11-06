//! What a session under drive is told once, as standing instructions: who speaks to it, how to
//! work inside the range it was given, and how to end each turn so the Router can read the
//! conclusion. The Router's step messages stay plain; this is where the working method lives.

use std::sync::Arc;

use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::WorldStateContributionInput;
use codex_extension_api::WorldStateSectionContribution;
use codex_extension_api::input_text;
use serde_json::Value;

use super::DriveService;

const RULES_ID: &str = "superagent_driven";
const RULES_MARKERS: (&str, &str) = ("<superagent_driven>", "</superagent_driven>");
/// Told again when this text changes.
const VERSION: &str = "2";

const RULES: &str = "\
这个会话由总 Router 代用户推进：每轮开头的话来自 Router，它转述用户的要求和反馈。
- 回合没有沙箱：可以开软件、开浏览器、联网。只改当前文件夹里的东西；不碰 ~/.codex、别的项目和技能目录；要推送、发布、删除或对外发消息，先在结论里问用户。
- 先确认环境：这里能用什么运行和检查结果；不能用的换个办法自己验证，能自己做的检查不推给用户。
- 在给定的范围内连续做完，不要每做一点就停下来问。
- 每轮结束时先说结论：做了什么、没做到什么、凭什么（怎么检查的）、卡在哪；说人话，不超过 300 字。
- 只有用户才能定的事（要什么、怎么取舍、要他提供的东西、最终验收）才问，写成一个具体问题放在最后。
- 项目有 .engineering 锚定记录时，按工程本体的要求回填阶段、决定和证据。";

type Lookup = Arc<dyn Fn() -> Option<DriveService> + Send + Sync>;

pub struct DriveContext {
    drive: Lookup,
}

impl DriveContext {
    pub fn new(drive: Lookup) -> Self {
        Self { drive }
    }
}

impl ContextContributor for DriveContext {
    fn contribute_world_state<'a>(
        &'a self,
        input: WorldStateContributionInput<'a>,
    ) -> ExtensionFuture<'a, Vec<WorldStateSectionContribution>> {
        let drive = (self.drive)();
        let thread_id = input.thread_store.level_id().to_owned();
        Box::pin(async move {
            let Some(drive) = drive else {
                return Vec::new();
            };
            match drive.record(&thread_id).await {
                Ok(Some(_)) => vec![rules_section()],
                Ok(None) => Vec::new(),
                Err(err) => {
                    tracing::warn!("superagent drive: rules for {thread_id}: {err:#}");
                    Vec::new()
                }
            }
        })
    }
}

fn rules_section() -> WorldStateSectionContribution {
    WorldStateSectionContribution::new(RULES_ID, move |previous| {
        let fragment = match previous {
            PreviousWorldStateSection::Known(Value::String(told)) if told == VERSION => None,
            PreviousWorldStateSection::Known(_)
            | PreviousWorldStateSection::Unknown
            | PreviousWorldStateSection::Absent => Some(RenderedWorldStateFragment::new(
                "developer",
                RULES_MARKERS,
                input_text(RULES),
            )),
        };
        (Some(Value::String(VERSION.to_owned())), fragment)
    })
    .with_retained_fragment_matcher(|role, text| {
        role == "developer" && text.trim_start().starts_with(RULES_MARKERS.0)
    })
}
