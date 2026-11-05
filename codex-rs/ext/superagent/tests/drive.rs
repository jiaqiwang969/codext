//! The drive loop: the Router's words go to the session; each finished turn's conclusion comes
//! back to the Router, after the audit for an anchored goal; a step the Router says is checked
//! against the records first; a step in flight is never sent twice, not even across a restart.

mod common;

use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use codex_protocol::ThreadId;
use codex_superagent_extension::BoxFut;
use codex_superagent_extension::Effects;
use codex_superagent_extension::HealthBoard;
use codex_superagent_extension::drive::AccountInfo;
use codex_superagent_extension::drive::AccountSource;
use codex_superagent_extension::drive::Audit;
use codex_superagent_extension::drive::DriveRecord;
use codex_superagent_extension::drive::DriveService;
use codex_superagent_extension::drive::DriveStart;
use codex_superagent_extension::drive::DriveState;
use codex_superagent_extension::drive::Feedback;
use codex_superagent_extension::drive::Goal;
use codex_superagent_extension::drive::DriveMode;
use codex_superagent_extension::drive::Judge;
use codex_superagent_extension::drive::Say;
use codex_superagent_extension::host::DeliveryMode;
use codex_superagent_extension::host::GoalStatus;
use codex_superagent_extension::host::SuperagentHost;
use codex_superagent_extension::host::ThreadSpec;
use codex_superagent_extension::host::TurnBrief;
use common::FakeClock;
use common::T0;
use common::open_store;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

const THREAD: &str = "019a0000-0000-7000-8000-00000000d001";
const ROUTER: &str = "019a0000-0000-7000-8000-00000000d0aa";
const OPENED: &str = "019a0000-0000-7000-8000-00000000d002";

#[derive(Default)]
struct FakeHost {
    calls: Mutex<Vec<String>>,
    delivered: Mutex<Vec<String>>,
    queue: Mutex<Vec<(String, String)>>,
    latest: Mutex<Option<TurnBrief>>,
    turns: Mutex<u32>,
    opened: Mutex<Vec<(PathBuf, Option<String>)>>,
    /// The accounts the session was bound to, in order; the last one is its current account.
    bound: Mutex<Vec<String>>,
    /// Like the real host, delivering to an idle session starts its turn: the drive's turn-start
    /// hook runs inside `deliver`.
    on_deliver: Mutex<Option<DriveService>>,
}

impl FakeHost {
    fn lock<T>(cell: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        cell.lock().unwrap_or_else(PoisonError::into_inner)
    }
    fn delivered(&self) -> Vec<String> {
        Self::lock(&self.delivered).clone()
    }
    /// The step's turn failed with an error of `kind`.
    fn fail(&self, kind: &str, message: &str) {
        let mut turns = Self::lock(&self.turns);
        *turns += 1;
        *Self::lock(&self.latest) = Some(TurnBrief {
            id: format!("turn-{turns}"),
            status: "failed".to_owned(),
            error: Some(message.to_owned()),
            error_kind: Some(kind.to_owned()),
            ..TurnBrief::default()
        });
    }
    /// A new turn of the session ended (or runs) with the worker's last words.
    fn finish(&self, status: &str, conclusion: &str) {
        let mut turns = Self::lock(&self.turns);
        *turns += 1;
        *Self::lock(&self.latest) = Some(TurnBrief {
            id: format!("turn-{turns}"),
            status: status.to_owned(),
            user: Vec::new(),
            assistant: vec!["先看看目录".to_owned(), conclusion.to_owned()],
            ..TurnBrief::default()
        });
    }
}

impl SuperagentHost for FakeHost {
    fn start_thread(&self, spec: ThreadSpec) -> BoxFut<'_, anyhow::Result<ThreadId>> {
        Self::lock(&self.opened).push((spec.cwd, spec.profile));
        Box::pin(async { Ok(ThreadId::from_string(OPENED)?) })
    }
    fn thread_profile(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Option<String>>> {
        let current = Self::lock(&self.bound)
            .last()
            .cloned()
            .unwrap_or_else(|| "lab-account".to_owned());
        Box::pin(async move { Ok(Some(current)) })
    }
    fn bind_profile(&self, _thread: ThreadId, profile: String) -> BoxFut<'_, anyhow::Result<()>> {
        Self::lock(&self.bound).push(profile);
        Box::pin(async { Ok(()) })
    }
    fn resume_thread(
        &self,
        thread: ThreadId,
        role: Option<String>,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        Self::lock(&self.calls).push(format!("resume:{thread}:{role:?}"));
        Box::pin(async { Ok(()) })
    }
    fn deliver(
        &self,
        thread: ThreadId,
        text: String,
        mode: DeliveryMode,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        Self::lock(&self.calls).push(format!("deliver:{thread}:{mode:?}"));
        Self::lock(&self.delivered).push(text);
        let drive = Self::lock(&self.on_deliver).clone();
        Box::pin(async move {
            if let Some(drive) = drive {
                drive.turn_started(&thread.to_string()).await?;
            }
            Ok(())
        })
    }
    fn start_step(&self, thread: ThreadId, text: String) -> BoxFut<'_, anyhow::Result<()>> {
        Self::lock(&self.calls).push(format!("step:{thread}"));
        Self::lock(&self.delivered).push(text);
        let drive = Self::lock(&self.on_deliver).clone();
        Box::pin(async move {
            if let Some(drive) = drive {
                drive.turn_started(&thread.to_string()).await?;
            }
            Ok(())
        })
    }
    fn session_turns(
        &self,
        _thread: ThreadId,
        _limit: u32,
    ) -> BoxFut<'_, anyhow::Result<Vec<TurnBrief>>> {
        let latest = Self::lock(&self.latest).clone();
        Box::pin(async move { Ok(latest.into_iter().collect()) })
    }
    fn queued(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<(String, String)>>> {
        let queue = Self::lock(&self.queue).clone();
        Box::pin(async move { Ok(queue) })
    }
    fn withdraw_queued(
        &self,
        _thread: ThreadId,
        item_id: String,
    ) -> BoxFut<'_, anyhow::Result<bool>> {
        let mut queue = Self::lock(&self.queue);
        let before = queue.len();
        queue.retain(|(id, _)| *id != item_id);
        let removed = queue.len() < before;
        Box::pin(async move { Ok(removed) })
    }
    fn interrupt(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn clean_background(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn background_commands(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<String>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn children(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Vec<ThreadId>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn goal_set(
        &self,
        _thread: ThreadId,
        _objective: String,
        _token_budget: Option<i64>,
    ) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn goal_clear(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn goal_status(&self, _thread: ThreadId) -> BoxFut<'_, anyhow::Result<Option<GoalStatus>>> {
        Box::pin(async { Ok(None) })
    }
}

/// Answers the scripted audits in order, the last one again once they run out.
#[derive(Default)]
struct ScriptedJudge {
    audits: Mutex<VecDeque<Audit>>,
    last: Mutex<Option<Audit>>,
}

impl ScriptedJudge {
    fn then(&self, audit: Audit) {
        FakeHost::lock(&self.audits).push_back(audit);
    }
}

impl Judge for ScriptedJudge {
    fn audit<'a>(&'a self, _project_home: &'a Path) -> BoxFut<'a, anyhow::Result<Audit>> {
        let next = FakeHost::lock(&self.audits).pop_front();
        let audit = match next {
            Some(audit) => {
                *FakeHost::lock(&self.last) = Some(audit.clone());
                audit
            }
            None => FakeHost::lock(&self.last).clone().unwrap_or_default(),
        };
        Box::pin(async move { Ok(audit) })
    }
}

/// The account pool the switching reads.
#[derive(Default)]
struct FakeAccounts {
    pool: Mutex<Vec<AccountInfo>>,
}

impl AccountSource for FakeAccounts {
    fn accounts(&self) -> BoxFut<'_, anyhow::Result<Vec<AccountInfo>>> {
        let pool = FakeHost::lock(&self.pool).clone();
        Box::pin(async move { Ok(pool) })
    }
}

fn account(name: &str, plan: &str, remaining: f64, resets_in_hours: i64) -> AccountInfo {
    AccountInfo {
        name: name.to_owned(),
        plan: plan.to_owned(),
        usable: true,
        remaining,
        resets_at: Some(chrono::Utc::now().timestamp() + resets_in_hours * 3600),
    }
}

fn audit(status: &str, verified: &[&str], open: &[&str], digest: &str) -> Audit {
    let ids = |ids: &[&str]| ids.iter().map(|id| (*id).to_owned()).collect();
    Audit {
        status: status.to_owned(),
        verified: ids(verified),
        open: ids(open),
        inconsistent: Vec::new(),
        findings: Vec::new(),
        project_sha256: format!("p-{digest}"),
        anchor_sha256: format!("a-{digest}"),
    }
}

/// A workspace whose current commit names a Project record and an anchor with two items.
fn workspace(dir: &Path) -> anyhow::Result<PathBuf> {
    let home = dir.join("notes/.engineering");
    std::fs::create_dir_all(home.join("commits/c1"))?;
    std::fs::create_dir_all(home.join("inputs"))?;
    std::fs::write(
        home.join("HEAD.json"),
        json!({ "commit": { "path": "commits/c1/commit.json" } }).to_string(),
    )?;
    std::fs::write(
        home.join("commits/c1/commit.json"),
        json!({ "proposal": {
            "project": { "path": "inputs/project-r1.json" },
            "anchor": { "path": "inputs/anchor-r1.json" },
        }})
        .to_string(),
    )?;
    std::fs::write(
        home.join("inputs/anchor-r1.json"),
        json!({
            "schema": "oev2.route-anchor/2",
            "deliverable": "一份按主题分好的笔记目录和索引",
            "requests": [
                { "id": "U1", "kind": "root", "text": "整理笔记目录" },
                { "id": "U2", "kind": "correction", "text": "索引要按日期", "affects": ["A2"] },
            ],
            "acceptance": [
                { "id": "A1", "statement": "每个主题一个子目录", "status": "open" },
                { "id": "A2", "statement": "根目录有索引文件", "status": "open" },
            ],
        })
        .to_string(),
    )?;
    std::fs::write(home.join("inputs/project-r1.json"), "{}")?;
    Ok(home)
}

const REQUEST: &str = "整理笔记目录，按主题分好";
const FIRST: &str = "把 notes 里的笔记按主题分进子目录，做完告诉我怎么检查。";

fn simple(folder: PathBuf) -> DriveStart {
    DriveStart {
        project_home: folder,
        title: "整理笔记".to_owned(),
        mode: DriveMode::Simple,
        max_steps: None,
        request: REQUEST.to_owned(),
        goal: None,
        first: FIRST.to_owned(),
    }
}

fn anchored(project_home: PathBuf, goal: Option<Goal>) -> DriveStart {
    DriveStart {
        mode: DriveMode::Anchored,
        request: goal
            .as_ref()
            .map(|goal| goal.request.clone())
            .unwrap_or_else(|| "整理笔记目录".to_owned()),
        goal,
        ..simple(project_home)
    }
}

struct Setup {
    _dir: TempDir,
    work: TempDir,
    home: PathBuf,
    host: Arc<FakeHost>,
    judge: Arc<ScriptedJudge>,
    accounts: Arc<FakeAccounts>,
    effects: Effects,
    drive: DriveService,
}

/// The effect queue is not started: each test runs the looks itself, in order.
async fn setup_with(stall: std::time::Duration) -> anyhow::Result<Setup> {
    let (dir, store) = open_store().await?;
    let work = TempDir::new()?;
    let home = workspace(work.path())?;
    let host = Arc::new(FakeHost::default());
    let judge = Arc::new(ScriptedJudge::default());
    let accounts = Arc::new(FakeAccounts::default());
    let effects =
        Effects::new(store.clone(), HealthBoard::new(), FakeClock::at(T0).clock()).await?;
    let drive = DriveService::install(
        store,
        effects.clone(),
        host.clone() as Arc<dyn SuperagentHost>,
        judge.clone() as Arc<dyn Judge>,
        Some(accounts.clone() as Arc<dyn AccountSource>),
        stall,
    )
    .await?;
    Ok(Setup { _dir: dir, work, home, host, judge, accounts, effects, drive })
}

async fn setup() -> anyhow::Result<Setup> {
    setup_with(std::time::Duration::from_secs(3600)).await
}

impl Setup {
    /// A simple drive in the work folder.
    async fn start(&self) -> anyhow::Result<DriveRecord> {
        self.drive.start(THREAD, simple(self.work.path().to_owned())).await
    }
    async fn start_anchored(&self) -> anyhow::Result<DriveRecord> {
        self.drive.start(THREAD, anchored(self.home.clone(), None)).await
    }
    async fn record(&self) -> anyhow::Result<DriveRecord> {
        Ok(self.drive.record(THREAD).await?.expect("a drive record"))
    }
    /// The step's turn starts and ends normally with the worker's conclusion; then the engine
    /// looks at it.
    async fn turn(&self, conclusion: &str) -> anyhow::Result<()> {
        self.drive.turn_started(THREAD).await?;
        self.host.finish("completed", conclusion);
        self.drive.turn_stopped(THREAD).await?;
        self.drive.step(THREAD).await
    }
    async fn notices(&self) -> anyhow::Result<Vec<String>> {
        Ok(self
            .effects
            .queued("notice")
            .await?
            .into_iter()
            .map(|(_, payload)| payload["summary"].as_str().unwrap_or_default().to_owned())
            .collect())
    }
}

fn say(text: &str, builds_on: &str, serves: &str) -> Say {
    Say {
        text: text.to_owned(),
        builds_on: builds_on.to_owned(),
        serves: serves.to_owned(),
        correction: None,
    }
}

#[tokio::test]
async fn start_sends_the_routers_first_message_and_records_it_in_flight() -> anyhow::Result<()> {
    let s = setup().await?;
    let mut plain = simple(s.work.path().to_owned());
    plain.first = "推进验收项 A1：按工程本体写阶段声明".to_owned();
    let refused = s.drive.start(THREAD, plain).await;
    assert!(format!("{:#}", refused.expect_err("jargon")).contains("方法名或编号"));
    let mut wordless = simple(s.work.path().to_owned());
    wordless.request = " ".to_owned();
    assert!(s.drive.start(THREAD, wordless).await.is_err());
    assert!(s.host.delivered().is_empty());

    let record = s.start().await?;
    assert_eq!(s.host.delivered(), vec![FIRST.to_owned()]);
    assert_eq!(
        (record.mode, record.steps, record.inflight, record.request.as_str()),
        (DriveMode::Simple, 1, Some(1), REQUEST)
    );
    assert_eq!(record.history[0].text, FIRST);
    assert!(s.start().await.is_err(), "a second start is refused");
    Ok(())
}

#[tokio::test]
async fn a_finished_turn_hands_its_conclusion_to_the_router_and_sends_nothing() -> anyhow::Result<()>
{
    let s = setup().await?;
    s.start().await?;
    s.turn("分好了 3 个主题目录；浏览器在沙箱里打不开，没法真实试看").await?;
    let record = s.record().await?;
    assert_eq!(
        record.state,
        DriveState::Reported {
            summary: "分好了 3 个主题目录；浏览器在沙箱里打不开，没法真实试看".to_owned()
        }
    );
    let report = record.report.expect("a report");
    assert_eq!((report.turn.as_str(), report.audit), ("turn-1", None));
    assert_eq!(s.host.delivered().len(), 1, "the engine wrote no step");
    let notices = s.notices().await?;
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("第 1 步结束"), "{}", notices[0]);
    assert!(notices[0].contains("浏览器在沙箱里打不开"), "{}", notices[0]);
    assert!(notices[0].contains("superagent_drive say"), "{}", notices[0]);
    Ok(())
}

#[tokio::test]
async fn say_goes_out_only_on_a_basis_found_in_the_records() -> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    let next = "浏览器开不了就用命令行列出目录树自己核对，每个笔记只在一个主题里。";
    // Handed over and not started yet: nothing more goes.
    let early = s.drive.say(THREAD, say(next, "打不开", "按主题分好")).await;
    assert!(format!("{:#}", early.expect_err("busy")).contains("还没开始"));
    s.turn("分好了 3 个主题目录；浏览器在沙箱里打不开").await?;

    let cases = [
        (say(next, "全部做完了", "按主题分好"), "builds_on"),
        (say(next, "浏览器在沙箱里打不开", "做一个网站"), "serves"),
        (say("验收项 A1 还没闭合", "浏览器在沙箱里打不开", "按主题分好"), "方法名或编号"),
    ];
    for (bad, why) in cases {
        let refused = s.drive.say(THREAD, bad).await;
        assert!(format!("{:#}", refused.expect_err(why)).contains(why), "{why}");
    }
    assert_eq!(s.host.delivered().len(), 1);

    let record = s
        .drive
        .say(THREAD, say(next, "浏览器 在沙箱里，打不开", "按主题分好"))
        .await?
        .expect("a record");
    assert_eq!(s.host.delivered().last().map(String::as_str), Some(next));
    assert_eq!((record.steps, record.inflight), (2, Some(2)));
    let step = record.history.last().expect("step 2");
    assert_eq!(
        (step.builds_on.as_str(), step.serves.as_str()),
        ("浏览器 在沙箱里，打不开", "按主题分好")
    );

    // The same words again, without anything new, do not go.
    s.turn("还是打不开浏览器").await?;
    let again = s.drive.say(THREAD, say(next, "还是打不开浏览器", "按主题分好")).await;
    assert!(format!("{:#}", again.expect_err("repeat")).contains("和第 2 步说的一样"));
    Ok(())
}

#[tokio::test]
async fn the_persons_words_are_checked_before_they_are_passed_on() -> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    s.turn("分好了 3 个主题目录").await?;
    let words = "主题目录名用英文，日期放前面";
    let next = "目录名改成英文，日期放在前面，改完再列一遍目录树。";
    let unheard = Say {
        correction: Some(words.to_owned()),
        ..say(next, "目录名用英文", "按主题分好")
    };
    let refused = s.drive.say(THREAD, unheard.clone()).await;
    assert!(format!("{:#}", refused.expect_err("unheard")).contains("原话"));

    s.drive.heard(&format!("{words}。")).await?;
    let record = s.drive.say(THREAD, unheard).await?.expect("a record");
    assert_eq!(record.corrections, vec![words.to_owned()]);
    assert_eq!(s.host.delivered().last().map(String::as_str), Some(next));
    Ok(())
}

#[tokio::test]
async fn what_the_person_types_into_the_session_is_kept_but_not_the_drives_own_step()
-> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    s.drive.person_input(THREAD, FIRST).await?;
    s.drive.person_input(THREAD, "子目录名用英文").await?;
    assert_eq!(s.record().await?.notes, vec!["子目录名用英文".to_owned()]);
    s.turn("分好了").await?;
    assert!(s.notices().await?[0].contains("用户在会话里说过：子目录名用英文"));
    // The person's own later turn replaces the conclusion the Router reads.
    s.host.finish("completed", "按你说的改成英文了");
    s.drive.turn_stopped(THREAD).await?;
    s.drive.step(THREAD).await?;
    let report = s.record().await?.report.expect("a report");
    assert_eq!((report.turn.as_str(), report.text.as_str()), ("turn-2", "按你说的改成英文了"));
    Ok(())
}

#[tokio::test]
async fn an_anchored_turn_is_judged_and_the_audit_may_stop_the_drive() -> anyhow::Result<()> {
    let s = setup().await?;
    s.start_anchored().await?;
    s.judge.then(audit("anchored", &["A1"], &["A2"], "1"));
    s.turn("分好了主题目录，索引还没写").await?;
    let record = s.record().await?;
    assert!(matches!(record.state, DriveState::Reported { .. }));
    assert_eq!(
        record.report.and_then(|report| report.audit).as_deref(),
        Some("anchored；已核实 A1；未闭合 A2；不一致 无")
    );
    // The basis may quote an acceptance item of the anchor.
    s.drive
        .say(THREAD, say("接着写根目录的索引，按日期排。", "索引还没写", "根目录有索引文件"))
        .await?;

    s.judge.then(audit("anchored", &["A1", "A2"], &[], "2"));
    s.turn("索引写好了").await?;
    assert_eq!(
        s.record().await?.state,
        DriveState::Deliverable {
            summary: "anchored；已核实 A1、A2；未闭合 无；不一致 无".to_owned()
        }
    );
    let notices = s.notices().await?;
    assert!(notices.last().expect("a notice").contains("可交付，等用户验收"));

    s.drive.resume(THREAD).await?;
    s.drive
        .say(THREAD, say("把索引里的日期格式统一一下。", "索引写好了", "索引要按日期"))
        .await?;
    let mut drift = audit("drift", &[], &["A2"], "3");
    drift.findings = vec!["code=no_progress_streak stage=S02".to_owned()];
    s.judge.then(drift);
    s.turn("改了格式").await?;
    assert_eq!(
        s.record().await?.state,
        DriveState::NeedsDecision {
            reason: "偏移：code=no_progress_streak stage=S02".to_owned()
        }
    );
    assert_eq!(s.host.delivered().len(), 3);
    Ok(())
}

#[tokio::test]
async fn the_step_cap_pauses_and_only_a_raised_cap_lets_the_router_go_on() -> anyhow::Result<()> {
    let s = setup().await?;
    let start = DriveStart {
        max_steps: Some(1),
        ..simple(s.work.path().to_owned())
    };
    s.drive.start(THREAD, start).await?;
    s.turn("分好了，还差索引").await?;
    assert_eq!(
        s.record().await?.state,
        DriveState::Paused {
            reason: "已推进 1 步，达到上限；放宽要问用户".to_owned()
        }
    );
    let next = say("接着写索引。", "还差索引", "按主题分好");
    let refused = s.drive.say(THREAD, next.clone()).await;
    assert!(format!("{:#}", refused.expect_err("cap")).contains("已推进 1/1 步"));
    s.drive.set_max_steps(THREAD, 3).await?;
    s.drive.resume(THREAD).await?;
    s.drive.say(THREAD, next).await?;
    assert_eq!(s.record().await?.steps, 2);
    Ok(())
}

#[tokio::test]
async fn an_interruption_or_a_failed_turn_pauses_the_drive() -> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    s.drive.turn_started(THREAD).await?;
    s.drive.turn_interrupted(THREAD).await?;
    assert!(matches!(s.record().await?.state, DriveState::Paused { .. }));
    assert_eq!(s.notices().await?.len(), 0, "the person's own pause is not told back");

    s.drive.stop(THREAD).await?;
    s.start().await?;
    s.drive.turn_started(THREAD).await?;
    s.host.finish("failed", "出错了");
    s.drive.step(THREAD).await?;
    assert_eq!(
        s.record().await?.state,
        DriveState::Paused {
            reason: "第 1 步没有正常结束（failed）".to_owned()
        }
    );
    Ok(())
}

#[tokio::test]
async fn a_step_that_never_starts_is_withdrawn_and_the_drive_pauses() -> anyhow::Result<()> {
    let s = setup_with(std::time::Duration::ZERO).await?;
    s.start().await?;
    *FakeHost::lock(&s.host.queue) = vec![
        ("q1".to_owned(), FIRST.to_owned()),
        ("q2".to_owned(), "用户自己排的话".to_owned()),
    ];
    s.drive.step(THREAD).await?;
    let record = s.record().await?;
    assert_eq!(
        (record.state, record.inflight, record.steps),
        (
            DriveState::Paused {
                reason: "第 1 步交出后一直没有开始，已撤回".to_owned()
            },
            None,
            0
        )
    );
    assert_eq!(
        FakeHost::lock(&s.host.queue).clone(),
        vec![("q2".to_owned(), "用户自己排的话".to_owned())]
    );
    Ok(())
}

#[tokio::test]
async fn recovery_and_stop_withdraw_only_the_drives_queued_steps() -> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    *FakeHost::lock(&s.host.queue) = vec![
        ("q1".to_owned(), FIRST.to_owned()),
        ("q2".to_owned(), "用户自己排的话".to_owned()),
    ];
    // A restart before the step's turn started: no replay, the queued step goes.
    s.drive.recover().await?;
    let record = s.record().await?;
    assert_eq!((record.steps, record.inflight), (0, None));
    assert!(matches!(record.state, DriveState::Paused { .. }));
    assert_eq!(FakeHost::lock(&s.host.queue).len(), 1);
    assert_eq!(s.host.delivered().len(), 1, "nothing was sent again");

    FakeHost::lock(&s.host.queue).push(("q3".to_owned(), "[推进 第9步 · 旧]".to_owned()));
    s.drive.stop(THREAD).await?;
    assert_eq!(
        FakeHost::lock(&s.host.queue).clone(),
        vec![("q2".to_owned(), "用户自己排的话".to_owned())]
    );
    assert!(s.drive.record(THREAD).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn handing_over_a_step_may_start_its_turn_at_once() -> anyhow::Result<()> {
    let s = setup().await?;
    *FakeHost::lock(&s.host.on_deliver) = Some(s.drive.clone());
    tokio::time::timeout(std::time::Duration::from_secs(5), s.start()).await??;
    let record = s.record().await?;
    assert_eq!((record.steps, record.inflight, record.started), (1, None, Some(1)));
    Ok(())
}

#[tokio::test]
async fn a_new_session_opens_in_its_folder_under_the_routers_account() -> anyhow::Result<()> {
    let s = setup().await?;
    let folder = s.work.path().join("01-snake");
    let relative = s.drive.open_session(Path::new("01-snake"), Some(ROUTER)).await;
    assert!(relative.is_err());
    let thread = s.drive.open_session(&folder, Some(ROUTER)).await?;
    assert_eq!(thread, OPENED);
    assert!(folder.is_dir());
    assert_eq!(
        FakeHost::lock(&s.host.opened).clone(),
        vec![(folder, Some("lab-account".to_owned()))]
    );
    Ok(())
}

#[test]
fn without_a_commit_the_plain_anchor_and_the_latest_project_record_are_read() -> anyhow::Result<()>
{
    let dir = TempDir::new()?;
    let home = dir.path();
    assert!(codex_superagent_extension::drive::workspace(home).is_err());
    std::fs::write(home.join("route-anchor.json"), "{}")?;
    std::fs::write(home.join("project.json"), "{}")?;
    let plain = codex_superagent_extension::drive::workspace(home)?;
    assert_eq!(
        (plain.project.as_str(), plain.anchor.as_str(), plain.committed),
        ("project.json", "route-anchor.json", false)
    );
    for name in ["project-r2.json", "project-r10.json"] {
        std::fs::write(home.join(name), "{}")?;
    }
    assert_eq!(
        codex_superagent_extension::drive::workspace(home)?.project,
        "project-r10.json"
    );
    Ok(())
}

#[test]
fn an_audit_report_reads_as_its_delivery_scope() -> anyhow::Result<()> {
    let report = json!({
        "schema": "oev2.route-audit/2",
        "status": "anchored",
        "delivery_verified": ["A1", "A2", "A3"],
        "delivery_open": [],
        "delivery_inconsistent": [],
        "open_acceptance": [],
        "findings": [],
        "project_sha256": "p",
        "anchor_sha256": "a",
    });
    let audit = Audit::parse(&report)?;
    assert!(audit.deliverable());
    assert_eq!(audit.summary(), "anchored；已核实 A1、A2、A3；未闭合 无；不一致 无");
    let failure = json!({
        "schema": "oev2.command-failure/1",
        "error": { "code": "invalid_contract", "reason": "input_or_output_contract_unavailable" },
    });
    assert!(Audit::parse(&failure).is_err());
    Ok(())
}

#[tokio::test]
async fn a_confirmed_goal_anchors_a_project_that_has_none() -> anyhow::Result<()> {
    let s = setup().await?;
    let work = TempDir::new()?;
    let home = work.path().join("231-cad-drawings/.engineering");
    let goal = Goal {
        request: "把图纸按零件号整理好".to_owned(),
        deliverable: "按零件号分好目录的图纸".to_owned(),
        acceptance: vec!["A1：每张图纸在其零件号目录下".to_owned(), "目录名就是零件号".to_owned()],
    };
    let partial = Goal {
        acceptance: Vec::new(),
        ..goal.clone()
    };
    assert!(s.drive.start(THREAD, anchored(home.clone(), Some(partial))).await.is_err());
    assert!(!home.join("route-anchor.json").exists());

    s.drive.start(THREAD, anchored(home.clone(), Some(goal.clone()))).await?;
    assert_eq!(std::fs::read_to_string(home.join("request-root.txt"))?, "把图纸按零件号整理好");
    let anchor = codex_superagent_extension::drive::Anchor::read(&home)?;
    assert_eq!(
        (anchor.deliverable.as_str(), anchor.acceptance.clone()),
        (
            "按零件号分好目录的图纸",
            vec![
                ("A1".to_owned(), "每张图纸在其零件号目录下".to_owned(), "open".to_owned()),
                ("A2".to_owned(), "目录名就是零件号".to_owned(), "open".to_owned()),
            ]
        )
    );
    // An existing anchor is used as it is, never rewritten.
    s.drive.stop(THREAD).await?;
    let other = Goal {
        deliverable: "别的交付物".to_owned(),
        ..goal
    };
    s.drive.start(THREAD, anchored(home.clone(), Some(other))).await?;
    let again = codex_superagent_extension::drive::Anchor::read(&home)?;
    assert_eq!(again.deliverable, "按零件号分好目录的图纸");
    Ok(())
}

#[tokio::test]
async fn the_persons_feedback_reopens_items_and_waits_for_the_router() -> anyhow::Result<()> {
    let s = setup().await?;
    let work = TempDir::new()?;
    let home = work.path().join("01-snake/.engineering");
    let goal = Goal {
        request: "建立一个项目，来做贪吃蛇的测试".to_owned(),
        deliverable: "可试玩的贪吃蛇".to_owned(),
        acceptance: vec!["方向键控制".to_owned(), "吃到食物变长加分".to_owned()],
    };
    s.drive.start(THREAD, anchored(home.clone(), Some(goal))).await?;
    let path = home.join("route-anchor.json");
    let mut anchor: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    anchor["acceptance"][1]["status"] = json!("closed");
    anchor["stages"] = json!([{ "id": "S01", "object_id": "root", "relation": "root",
        "relation_target": "root", "advances": ["A1"], "outcome": "no_progress" }]);
    std::fs::write(&path, anchor.to_string())?;
    s.judge.then(audit("drift", &[], &["A1"], "1"));
    s.turn("方向键做好了，请你在浏览器里试玩").await?;
    assert!(matches!(s.record().await?.state, DriveState::NeedsDecision { .. }));

    let words = "方向键能用，但是吃不到东西，也没有积分";
    let feedback = Feedback {
        text: words.to_owned(),
        passed: vec!["A1".to_owned()],
        failed: vec!["A2".to_owned()],
    };
    // Only the person's own words go into the anchor.
    assert!(s.drive.feedback(THREAD, feedback.clone()).await.is_err());
    s.drive.heard(words).await?;
    s.drive.feedback(THREAD, feedback).await?;
    let anchor: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    assert_eq!(
        (anchor["requests"][1]["text"].clone(), anchor["acceptance"][1]["status"].clone()),
        (json!(words), json!("open"))
    );
    let record = s.record().await?;
    assert_eq!(record.corrections, vec![words.to_owned()]);
    assert!(matches!(record.state, DriveState::Reported { .. }));
    assert_eq!(s.host.delivered().len(), 1, "the Router speaks next, not the engine");
    s.drive
        .say(THREAD, say("先把吃食物和计分做上，用命令行测试自己跑通。", "吃不到东西", "吃到食物变长加分"))
        .await?;
    assert_eq!(s.host.delivered().len(), 2);
    Ok(())
}

#[tokio::test]
async fn while_a_step_runs_say_is_steered_into_it_and_interrupt_pauses() -> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    let next = say("方向错了，换成另一个仓库重新查。", "整理笔记目录，按主题分好", "按主题分好");
    // Handed over but not started: too early.
    let early = s.drive.say(THREAD, next.clone()).await;
    assert!(format!("{:#}", early.expect_err("not started")).contains("还没开始"));
    s.drive.turn_started(THREAD).await?;
    s.host.finish("inProgress", "在查");
    let record = s.drive.say(THREAD, next.clone()).await?.expect("a record");
    let last = record.history.last().expect("the steered step");
    assert_eq!((record.steps, record.state.clone(), last.n, last.steered), (1, DriveState::Driving, 1, true));
    assert_eq!(
        FakeHost::lock(&s.host.calls).last().map(String::as_str),
        Some(&format!("deliver:{THREAD}:Steer")[..])
    );
    // The steered words come back as the session's input and are not kept as a note.
    s.drive.person_input(THREAD, &next.text).await?;
    assert!(s.record().await?.notes.is_empty());
    // The turn ends normally afterwards: its conclusion is read as usual.
    s.host.finish("completed", "换好了");
    s.drive.step(THREAD).await?;
    assert!(matches!(s.record().await?.state, DriveState::Reported { .. }));

    // The Router stops a running step: the drive pauses and the host interrupts the turn.
    s.drive.say(THREAD, say("接着做。", "换好了", "按主题分好")).await?;
    s.drive.turn_started(THREAD).await?;
    let paused = s.drive.interrupt(THREAD).await?.expect("a record");
    assert_eq!(
        paused.state,
        DriveState::Paused {
            reason: "第 2 步被打断；交回后说下一句".to_owned()
        }
    );
    s.drive.turn_interrupted(THREAD).await?;
    assert_eq!(s.record().await?.state, paused.state, "the hook keeps the Router's reason");
    Ok(())
}

#[tokio::test]
async fn a_step_failing_on_its_account_goes_again_on_the_account_that_resets_soonest()
-> anyhow::Result<()> {
    let s = setup().await?;
    *FakeHost::lock(&s.accounts.pool) = vec![
        account("free-soonest", "free", 100.0, 1),
        account("almost-empty", "pro", 2.0, 2),
        account("later", "pro", 90.0, 48),
        account("sooner", "pro", 80.0, 24),
        account("lab-account", "pro", 60.0, 6),
    ];
    s.start().await?;

    // The model is at capacity on the session's account: it cools down, the next goes.
    s.drive.turn_started(THREAD).await?;
    s.host.fail("serverOverloaded", "Selected model is at capacity. Please try a different model.");
    s.drive.step(THREAD).await?;
    let record = s.record().await?;
    assert_eq!(
        (record.state.clone(), record.steps, record.inflight, record.account.clone()),
        (DriveState::Driving, 1, Some(1), Some("sooner".to_owned()))
    );
    assert_eq!(FakeHost::lock(&s.host.bound).clone(), vec!["sooner".to_owned()]);
    assert_eq!(s.host.delivered(), vec![FIRST.to_owned(), FIRST.to_owned()], "the same step again");
    let cooling = s.drive.cooldowns().await?;
    let until = cooling.get("lab-account").expect("cooling down").until;
    let left = until - chrono::Utc::now().timestamp();
    assert!((14 * 60..=15 * 60).contains(&left), "{left}");
    let notices = s.notices().await?;
    assert!(notices.last().expect("a notice").contains("已换到 sooner"), "{notices:?}");

    // Out of quota there too: it cools down until its reset, and the step goes on the next.
    s.drive.turn_started(THREAD).await?;
    s.host.fail("usageLimitExceeded", "You've hit your usage limit.");
    s.drive.step(THREAD).await?;
    assert_eq!(s.record().await?.account.as_deref(), Some("later"));
    let sooner_reset = FakeHost::lock(&s.accounts.pool)[3].resets_at;
    assert_eq!(s.drive.cooldowns().await?.get("sooner").map(|c| c.until), sooner_reset);

    // Nothing left that is paid, has quota and is not cooling down: the drive pauses.
    s.drive.turn_started(THREAD).await?;
    s.host.fail("serverOverloaded", "Selected model is at capacity. Please try a different model.");
    s.drive.step(THREAD).await?;
    match s.record().await?.state {
        DriveState::Paused { reason } => assert!(reason.contains("没有可换的账号"), "{reason}"),
        other => panic!("not paused: {other:?}"),
    }
    assert_eq!(s.host.delivered().len(), 3);
    Ok(())
}

#[tokio::test]
async fn other_failures_pause_with_their_error_and_the_router_can_switch_accounts()
-> anyhow::Result<()> {
    let s = setup().await?;
    *FakeHost::lock(&s.accounts.pool) = vec![
        account("lab-account", "pro", 60.0, 6),
        account("sooner", "pro", 80.0, 24),
        account("later", "pro", 90.0, 48),
    ];
    s.start().await?;
    s.drive.turn_started(THREAD).await?;
    s.host.fail("contextWindowExceeded", "context window exceeded");
    s.drive.step(THREAD).await?;
    assert_eq!(
        s.record().await?.state,
        DriveState::Paused {
            reason: "第 1 步没有正常结束（failed：context window exceeded）".to_owned()
        }
    );
    assert!(FakeHost::lock(&s.host.bound).is_empty());

    let (record, label) = s.drive.switch_account(THREAD, Some("later".to_owned())).await?;
    assert!(label.starts_with("later（剩 90%"), "{label}");
    assert_eq!(record.and_then(|record| record.account), Some("later".to_owned()));
    let (_, label) = s.drive.switch_account(THREAD, None).await?;
    assert!(label.starts_with("lab-account"), "the next by reset time, not the current one: {label}");
    assert!(s.drive.switch_account(THREAD, Some("nobody".to_owned())).await.is_err());

    s.host.finish("inProgress", "在做");
    let busy = s.drive.switch_account(THREAD, None).await;
    assert!(format!("{:#}", busy.expect_err("running")).contains("先 interrupt"));
    Ok(())
}

#[test]
fn the_account_status_report_reads_as_a_pool() -> anyhow::Result<()> {
    let report = json!({ "accounts": [
        { "account": "kalliesallerwu", "status": "live", "auth_status": "ready", "plan": "pro",
          "quota": [ { "remaining_percent": 84.0, "resets_at": 1793935156 },
                     { "remaining_percent": 40.0, "resets_at": 1793900000 } ], "error": null },
        { "account": "OmarGuthorn8", "status": "live", "auth_status": "ready", "plan": "free",
          "quota": [ { "remaining_percent": 100.0, "resets_at": 1796000000 } ], "error": null },
        { "account": "broken", "status": "error", "auth_status": "expired", "plan": "pro",
          "quota": [], "error": "refresh failed" },
    ]});
    let pool = codex_superagent_extension::drive::parse_pool(&report);
    assert_eq!(
        pool,
        vec![
            AccountInfo { name: "kalliesallerwu".to_owned(), plan: "pro".to_owned(), usable: true,
                remaining: 40.0, resets_at: Some(1793900000) },
            AccountInfo { name: "OmarGuthorn8".to_owned(), plan: "free".to_owned(), usable: true,
                remaining: 100.0, resets_at: Some(1796000000) },
            AccountInfo { name: "broken".to_owned(), plan: "pro".to_owned(), usable: false,
                remaining: 0.0, resets_at: None },
        ]
    );
    let chosen = codex_superagent_extension::drive::choose(
        &pool, &std::collections::BTreeMap::new(), 0, None,
    );
    assert_eq!(chosen.map(|account| account.name.as_str()), Some("kalliesallerwu"));
    Ok(())
}
