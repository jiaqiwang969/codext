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
use codex_superagent_extension::Store;
use codex_superagent_extension::drive::AccountInfo;
use codex_superagent_extension::drive::AccountSource;
use codex_superagent_extension::drive::Audit;
use codex_superagent_extension::drive::DriveMode;
use codex_superagent_extension::drive::DriveRecord;
use codex_superagent_extension::drive::DriveService;
use codex_superagent_extension::drive::DriveStart;
use codex_superagent_extension::drive::DriveState;
use codex_superagent_extension::drive::Feedback;
use codex_superagent_extension::drive::Goal;
use codex_superagent_extension::drive::Judge;
use codex_superagent_extension::drive::Say;
use codex_superagent_extension::drive::StandingScope;
use codex_superagent_extension::drive::WaitOn;
use codex_superagent_extension::drive::drive_key;
use codex_superagent_extension::host::DeliveryMode;
use codex_superagent_extension::host::GoalStatus;
use codex_superagent_extension::host::StepStart;
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
    /// The latest turn (the one that ran) ends with the worker's last words.
    fn end_latest(&self, status: &str, conclusion: &str) {
        if let Some(turn) = Self::lock(&self.latest).as_mut() {
            turn.status = status.to_owned();
            turn.assistant = vec![conclusion.to_owned()];
        }
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
    fn start_step(&self, thread: ThreadId, text: String) -> BoxFut<'_, anyhow::Result<StepStart>> {
        Self::lock(&self.calls).push(format!("step:{thread}"));
        Self::lock(&self.delivered).push(text);
        let drive = Self::lock(&self.on_deliver).clone();
        // Like the real host: a turn that runs already takes the step's words.
        let running = Self::lock(&self.latest)
            .as_ref()
            .filter(|turn| turn.status == "inProgress")
            .map(|turn| turn.id.clone());
        Box::pin(async move {
            if let Some(turn) = running {
                return Ok(StepStart::Steered { turn: Some(turn) });
            }
            if let Some(drive) = drive {
                drive.turn_started(&thread.to_string()).await?;
            }
            Ok(StepStart::Started { turn: None })
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
    /// The next audit fails with this error.
    broken: Mutex<Option<String>>,
}

impl ScriptedJudge {
    fn then(&self, audit: Audit) {
        FakeHost::lock(&self.audits).push_back(audit);
    }
    fn break_next(&self, error: &str) {
        *FakeHost::lock(&self.broken) = Some(error.to_owned());
    }
}

impl Judge for ScriptedJudge {
    fn audit<'a>(&'a self, _project_home: &'a Path) -> BoxFut<'a, anyhow::Result<Audit>> {
        if let Some(error) = FakeHost::lock(&self.broken).take() {
            return Box::pin(async move { Err(anyhow::anyhow!(error)) });
        }
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
        account: None,
        router: None,
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
    store: Store,
    /// Where the drives' events go (`setup_routed` only).
    events: PathBuf,
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
        store.clone(),
        effects.clone(),
        host.clone() as Arc<dyn SuperagentHost>,
        judge.clone() as Arc<dyn Judge>,
        Some(accounts.clone() as Arc<dyn AccountSource>),
        stall,
    )
    .await?;
    let events = dir.path().join("events.jsonl");
    Ok(Setup {
        _dir: dir,
        work,
        home,
        host,
        judge,
        accounts,
        effects,
        drive,
        store,
        events,
    })
}

async fn setup() -> anyhow::Result<Setup> {
    setup_with(std::time::Duration::from_secs(3600)).await
}

/// Like `setup`, with Routers registered (the first is the one drives without a Router of their
/// own report to).
async fn setup_routed(routers: &[&str]) -> anyhow::Result<Setup> {
    let (dir, store) = open_store().await?;
    let work = TempDir::new()?;
    let home = workspace(work.path())?;
    let host = Arc::new(FakeHost::default());
    let judge = Arc::new(ScriptedJudge::default());
    let accounts = Arc::new(FakeAccounts::default());
    let effects =
        Effects::new(store.clone(), HealthBoard::new(), FakeClock::at(T0).clock()).await?;
    let routers: Vec<String> = routers.iter().map(|router| (*router).to_owned()).collect();
    let events = dir.path().join("events.jsonl");
    let drive = DriveService::install_with(
        store.clone(),
        effects.clone(),
        host.clone() as Arc<dyn SuperagentHost>,
        judge.clone() as Arc<dyn Judge>,
        codex_superagent_extension::drive::DriveOptions {
            accounts: Some(accounts.clone() as Arc<dyn AccountSource>),
            stall: std::time::Duration::from_secs(3600),
            policy: codex_superagent_extension::drive::AccountPolicy::default(),
            routers: Arc::new(move || routers.clone()),
            retry_after: std::time::Duration::ZERO,
            events: Some(events.clone()),
        },
    )
    .await?;
    Ok(Setup {
        _dir: dir,
        work,
        home,
        host,
        judge,
        accounts,
        effects,
        drive,
        store,
        events,
    })
}

impl Setup {
    /// Changes a record behind the engine's back, as time passing would.
    async fn rewrite(
        &self,
        thread: &str,
        change: impl FnOnce(&mut DriveRecord),
    ) -> anyhow::Result<()> {
        let key = drive_key(thread);
        let mut record: DriveRecord = self.store.kv_get(&key).await?.expect("a drive record");
        change(&mut record);
        self.store
            .atomic(async |tx| tx.kv_set(&key, &record).await)
            .await
    }
    /// A simple drive in the work folder.
    async fn start(&self) -> anyhow::Result<DriveRecord> {
        self.drive
            .start(THREAD, simple(self.work.path().to_owned()))
            .await
    }
    async fn start_anchored(&self) -> anyhow::Result<DriveRecord> {
        self.drive
            .start(THREAD, anchored(self.home.clone(), None))
            .await
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
            .queued(codex_superagent_extension::drive::KIND_NOTICE)
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
        ..Say::default()
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
        (
            record.mode,
            record.steps,
            record.inflight,
            record.request.as_str()
        ),
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
    s.turn("分好了 3 个主题目录；浏览器在沙箱里打不开，没法真实试看")
        .await?;
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
    assert!(
        notices[0].contains("浏览器在沙箱里打不开"),
        "{}",
        notices[0]
    );
    assert!(
        notices[0].contains("superagent_drive say"),
        "{}",
        notices[0]
    );
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
        (
            say("验收项 A1 还没闭合", "浏览器在沙箱里打不开", "按主题分好"),
            "方法名或编号",
        ),
    ];
    for (bad, why) in cases {
        let refused = s.drive.say(THREAD, bad).await;
        assert!(
            format!("{:#}", refused.expect_err(why)).contains(why),
            "{why}"
        );
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
    let again = s
        .drive
        .say(THREAD, say(next, "还是打不开浏览器", "按主题分好"))
        .await;
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
    assert_eq!(
        (report.turn.as_str(), report.text.as_str()),
        ("turn-2", "按你说的改成英文了")
    );
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
        .say(
            THREAD,
            say(
                "接着写根目录的索引，按日期排。",
                "索引还没写",
                "根目录有索引文件",
            ),
        )
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
    assert!(
        notices
            .last()
            .expect("a notice")
            .contains("可交付，等用户验收")
    );

    s.drive.resume(THREAD).await?;
    s.drive
        .say(
            THREAD,
            say("把索引里的日期格式统一一下。", "索引写好了", "索引要按日期"),
        )
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
    assert_eq!(
        s.notices().await?.len(),
        0,
        "the person's own pause is not told back"
    );

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
    assert_eq!(
        (record.steps, record.inflight, record.started),
        (1, None, Some(1))
    );
    Ok(())
}

#[tokio::test]
async fn a_new_session_opens_in_its_folder_under_the_routers_account() -> anyhow::Result<()> {
    let s = setup().await?;
    let folder = s.work.path().join("01-snake");
    let relative = s
        .drive
        .open_session(Path::new("01-snake"), Some(ROUTER))
        .await;
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
        (
            plain.project.as_str(),
            plain.anchor.as_str(),
            plain.committed
        ),
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
    assert_eq!(
        audit.summary(),
        "anchored；已核实 A1、A2、A3；未闭合 无；不一致 无"
    );
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
        acceptance: vec![
            "A1：每张图纸在其零件号目录下".to_owned(),
            "目录名就是零件号".to_owned(),
        ],
    };
    let partial = Goal {
        acceptance: Vec::new(),
        ..goal.clone()
    };
    assert!(
        s.drive
            .start(THREAD, anchored(home.clone(), Some(partial)))
            .await
            .is_err()
    );
    assert!(!home.join("route-anchor.json").exists());

    s.drive
        .start(THREAD, anchored(home.clone(), Some(goal.clone())))
        .await?;
    assert_eq!(
        std::fs::read_to_string(home.join("request-root.txt"))?,
        "把图纸按零件号整理好"
    );
    let anchor = codex_superagent_extension::drive::Anchor::read(&home)?;
    assert_eq!(
        (anchor.deliverable.as_str(), anchor.acceptance.clone()),
        (
            "按零件号分好目录的图纸",
            vec![
                (
                    "A1".to_owned(),
                    "每张图纸在其零件号目录下".to_owned(),
                    "open".to_owned()
                ),
                (
                    "A2".to_owned(),
                    "目录名就是零件号".to_owned(),
                    "open".to_owned()
                ),
            ]
        )
    );
    // An existing anchor is used as it is, never rewritten.
    s.drive.stop(THREAD).await?;
    let other = Goal {
        deliverable: "别的交付物".to_owned(),
        ..goal
    };
    s.drive
        .start(THREAD, anchored(home.clone(), Some(other)))
        .await?;
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
    s.drive
        .start(THREAD, anchored(home.clone(), Some(goal)))
        .await?;
    let path = home.join("route-anchor.json");
    let mut anchor: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    anchor["acceptance"][1]["status"] = json!("closed");
    anchor["stages"] = json!([{ "id": "S01", "object_id": "root", "relation": "root",
        "relation_target": "root", "advances": ["A1"], "outcome": "no_progress" }]);
    std::fs::write(&path, anchor.to_string())?;
    s.judge.then(audit("drift", &[], &["A1"], "1"));
    s.turn("方向键做好了，请你在浏览器里试玩").await?;
    assert!(matches!(
        s.record().await?.state,
        DriveState::NeedsDecision { .. }
    ));

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
        (
            anchor["requests"][1]["text"].clone(),
            anchor["acceptance"][1]["status"].clone()
        ),
        (json!(words), json!("open"))
    );
    let record = s.record().await?;
    assert_eq!(record.corrections, vec![words.to_owned()]);
    assert!(matches!(record.state, DriveState::Reported { .. }));
    assert_eq!(
        s.host.delivered().len(),
        1,
        "the Router speaks next, not the engine"
    );
    s.drive
        .say(
            THREAD,
            say(
                "先把吃食物和计分做上，用命令行测试自己跑通。",
                "吃不到东西",
                "吃到食物变长加分",
            ),
        )
        .await?;
    assert_eq!(s.host.delivered().len(), 2);
    Ok(())
}

#[tokio::test]
async fn while_a_step_runs_say_is_steered_into_it_and_interrupt_pauses() -> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    let next = say(
        "方向错了，换成另一个仓库重新查。",
        "整理笔记目录，按主题分好",
        "按主题分好",
    );
    // Handed over but not started: too early.
    let early = s.drive.say(THREAD, next.clone()).await;
    assert!(format!("{:#}", early.expect_err("not started")).contains("还没开始"));
    s.drive.turn_started(THREAD).await?;
    s.host.finish("inProgress", "在查");
    let record = s.drive.say(THREAD, next.clone()).await?.expect("a record");
    let last = record.history.last().expect("the steered step");
    assert_eq!(
        (record.steps, record.state.clone(), last.n, last.steered),
        (1, DriveState::Driving, 1, true)
    );
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
    assert!(matches!(
        s.record().await?.state,
        DriveState::Reported { .. }
    ));

    // The Router stops a running step: the drive pauses and the host interrupts the turn.
    s.drive
        .say(THREAD, say("接着做。", "换好了", "按主题分好"))
        .await?;
    s.drive.turn_started(THREAD).await?;
    let paused = s.drive.interrupt(THREAD).await?.expect("a record");
    assert_eq!(
        paused.state,
        DriveState::Paused {
            reason: "第 2 步被打断；交回后说下一句".to_owned()
        }
    );
    s.drive.turn_interrupted(THREAD).await?;
    assert_eq!(
        s.record().await?.state,
        paused.state,
        "the hook keeps the Router's reason"
    );
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
    s.host.fail(
        "serverOverloaded",
        "Selected model is at capacity. Please try a different model.",
    );
    s.drive.step(THREAD).await?;
    let record = s.record().await?;
    assert_eq!(
        (
            record.state.clone(),
            record.steps,
            record.inflight,
            record.account.clone()
        ),
        (DriveState::Driving, 1, Some(1), Some("sooner".to_owned()))
    );
    assert_eq!(
        FakeHost::lock(&s.host.bound).clone(),
        vec!["sooner".to_owned()]
    );
    assert_eq!(
        s.host.delivered(),
        vec![FIRST.to_owned(), FIRST.to_owned()],
        "the same step again"
    );
    let cooling = s.drive.cooldowns().await?;
    let until = cooling.get("lab-account").expect("cooling down").until;
    let left = until - chrono::Utc::now().timestamp();
    assert!((14 * 60..=15 * 60).contains(&left), "{left}");
    let notices = s.notices().await?;
    assert!(
        notices.last().expect("a notice").contains("已换到 sooner"),
        "{notices:?}"
    );

    // Out of quota there too: it cools down until its reset, and the step goes on the next.
    s.drive.turn_started(THREAD).await?;
    s.host
        .fail("usageLimitExceeded", "You've hit your usage limit.");
    s.drive.step(THREAD).await?;
    assert_eq!(s.record().await?.account.as_deref(), Some("later"));
    let sooner_reset = FakeHost::lock(&s.accounts.pool)[3].resets_at;
    assert_eq!(
        s.drive.cooldowns().await?.get("sooner").map(|c| c.until),
        sooner_reset
    );

    // Nothing left that is paid, has quota and is not cooling down: the drive pauses.
    s.drive.turn_started(THREAD).await?;
    s.host.fail(
        "serverOverloaded",
        "Selected model is at capacity. Please try a different model.",
    );
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
    s.host
        .fail("contextWindowExceeded", "context window exceeded");
    s.drive.step(THREAD).await?;
    assert_eq!(
        s.record().await?.state,
        DriveState::Paused {
            reason: "第 1 步没有正常结束（failed：context window exceeded）".to_owned()
        }
    );
    assert!(FakeHost::lock(&s.host.bound).is_empty());

    let (record, label) = s
        .drive
        .switch_account(THREAD, Some("later".to_owned()))
        .await?;
    assert!(label.starts_with("later（剩 90%"), "{label}");
    assert_eq!(
        record.and_then(|record| record.account),
        Some("later".to_owned())
    );
    let (_, label) = s.drive.switch_account(THREAD, None).await?;
    assert!(
        label.starts_with("lab-account"),
        "the next by reset time, not the current one: {label}"
    );
    assert!(
        s.drive
            .switch_account(THREAD, Some("nobody".to_owned()))
            .await
            .is_err()
    );

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
            AccountInfo {
                name: "kalliesallerwu".to_owned(),
                plan: "pro".to_owned(),
                usable: true,
                remaining: 40.0,
                resets_at: Some(1793900000)
            },
            AccountInfo {
                name: "OmarGuthorn8".to_owned(),
                plan: "free".to_owned(),
                usable: true,
                remaining: 100.0,
                resets_at: Some(1796000000)
            },
            AccountInfo {
                name: "broken".to_owned(),
                plan: "pro".to_owned(),
                usable: false,
                remaining: 0.0,
                resets_at: None
            },
        ]
    );
    let chosen = codex_superagent_extension::drive::choose(
        &pool,
        &std::collections::BTreeMap::new(),
        0,
        None,
    );
    assert_eq!(
        chosen.map(|account| account.name.as_str()),
        Some("kalliesallerwu")
    );
    Ok(())
}

#[tokio::test]
async fn a_router_turn_failing_on_its_account_moves_the_router_and_hands_it_the_words_again()
-> anyhow::Result<()> {
    let s = setup().await?;
    *FakeHost::lock(&s.accounts.pool) = vec![
        account("lab-account", "pro", 60.0, 6),
        account("sooner", "pro", 80.0, 24),
    ];
    let words = "看看 solidcraft 上游最近有什么新进展";
    let mut turns = FakeHost::lock(&s.host.turns);
    *turns += 1;
    drop(turns);
    *FakeHost::lock(&s.host.latest) = Some(TurnBrief {
        id: "router-turn-1".to_owned(),
        status: "failed".to_owned(),
        user: vec![words.to_owned()],
        error: Some("Selected model is at capacity. Please try a different model.".to_owned()),
        error_kind: Some("serverOverloaded".to_owned()),
        ..TurnBrief::default()
    });
    s.drive.router_turn_failed(THREAD).await?;
    assert_eq!(
        FakeHost::lock(&s.host.bound).clone(),
        vec!["sooner".to_owned()]
    );
    let sent = s.host.delivered();
    assert_eq!(sent.len(), 1);
    assert!(
        sent[0].starts_with("[通知] Router 的账号模型满载"),
        "{}",
        sent[0]
    );
    assert!(
        sent[0].contains("已换到 sooner") && sent[0].ends_with(words),
        "{}",
        sent[0]
    );
    assert_eq!(
        FakeHost::lock(&s.host.calls).last().map(String::as_str),
        Some(&format!("deliver:{THREAD}:Steer")[..])
    );
    assert!(s.drive.cooldowns().await?.contains_key("lab-account"));
    // The same failed turn reported again moves nothing.
    s.drive.router_turn_failed(THREAD).await?;
    assert_eq!(s.host.delivered().len(), 1);
    // A turn that ended normally is left alone.
    s.host.finish("completed", "好的");
    s.drive.router_turn_failed(THREAD).await?;
    assert_eq!(FakeHost::lock(&s.host.bound).len(), 1);
    Ok(())
}

#[tokio::test]
async fn a_look_before_the_end_reached_the_history_waits_and_then_reads_it() -> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    s.drive.turn_started(THREAD).await?;
    // The stop hook ran, and the history still shows the turn running: the look fails, so the
    // effect queue runs it again shortly, instead of dropping it and leaving the drive stuck.
    s.host.finish("inProgress", "整理到一半");
    s.drive.turn_stopped(THREAD).await?;
    let early = s.drive.step(THREAD).await;
    assert!(format!("{:#}", early.expect_err("not settled")).contains("还没写进会话记录"));
    assert_eq!(s.record().await?.state, DriveState::Driving);
    // Neither may the Router steer into a turn that has ended.
    let next = say("接着做索引。", "整理笔记目录", "按主题分好");
    let refused = s.drive.say(THREAD, next).await;
    assert!(format!("{:#}", refused.expect_err("ended")).contains("正在读它的结论"));
    // The history caught up: the same look reads the conclusion.
    *FakeHost::lock(&s.host.latest) = Some(TurnBrief {
        id: "turn-1".to_owned(),
        status: "completed".to_owned(),
        assistant: vec!["分好了 3 个主题目录".to_owned()],
        ..TurnBrief::default()
    });
    s.drive.step(THREAD).await?;
    assert_eq!(
        s.record().await?.state,
        DriveState::Reported {
            summary: "分好了 3 个主题目录".to_owned()
        }
    );
    Ok(())
}

#[tokio::test]
async fn a_failure_the_history_shows_as_an_interruption_is_read_from_the_error_hook()
-> anyhow::Result<()> {
    let s = setup().await?;
    *FakeHost::lock(&s.accounts.pool) = vec![
        account("lab-account", "pro", 60.0, 6),
        account("sooner", "pro", 80.0, 24),
    ];
    s.start().await?;
    s.drive.turn_started(THREAD).await?;
    // The account ran out; the session stopped before the error reached the history, which
    // shows the turn interrupted and without its error.
    s.drive.turn_failed(
        THREAD,
        "turn-1",
        &codex_protocol::protocol::CodexErrorInfo::UsageLimitExceeded,
        "You've hit your usage limit.",
    );
    s.host.finish("interrupted", "");
    s.drive.turn_stopped(THREAD).await?;
    s.drive.step(THREAD).await?;
    let record = s.record().await?;
    assert_eq!(
        (record.state, record.account),
        (DriveState::Driving, Some("sooner".to_owned())),
        "the step goes again on another account"
    );
    assert_eq!(s.host.delivered(), vec![FIRST.to_owned(), FIRST.to_owned()]);
    Ok(())
}

#[tokio::test]
async fn a_step_that_broke_off_on_the_network_goes_again_once() -> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    s.drive.turn_started(THREAD).await?;
    s.host.fail(
        "other",
        "stream disconnected before completion: error decoding response body",
    );
    s.drive.step(THREAD).await?;
    let record = s.record().await?;
    match &record.state {
        DriveState::Paused { reason } => {
            assert!(
                reason.ends_with(codex_superagent_extension::drive::RETRYING),
                "{reason}"
            )
        }
        other => panic!("not paused for the resend: {other:?}"),
    }
    assert_eq!(record.retried, Some(1));
    assert!(
        s.notices().await?.is_empty(),
        "the resend is the engine's own business"
    );
    s.drive.resend_step(THREAD, 1).await?;
    let record = s.record().await?;
    assert_eq!(
        (record.state, record.steps, record.inflight),
        (DriveState::Driving, 1, Some(1))
    );
    assert_eq!(s.host.delivered(), vec![FIRST.to_owned(), FIRST.to_owned()]);
    // The same step breaks off again: the drive pauses with the error.
    s.drive.turn_started(THREAD).await?;
    s.host.fail("other", "workspace routing discovery failed");
    s.drive.step(THREAD).await?;
    assert_eq!(
        s.record().await?.state,
        DriveState::Paused {
            reason: "第 1 步没有正常结束（failed：workspace routing discovery failed）".to_owned()
        }
    );
    Ok(())
}

#[tokio::test]
async fn a_resend_after_the_router_spoke_does_nothing() -> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    s.drive.turn_started(THREAD).await?;
    s.host.fail(
        "responseStreamDisconnected",
        "stream disconnected before completion",
    );
    s.drive.step(THREAD).await?;
    // The Router hands the session back before the resend comes.
    s.drive.resume(THREAD).await?;
    s.drive.resend_step(THREAD, 1).await?;
    assert_eq!(s.host.delivered().len(), 1, "nothing was sent again");
    assert!(matches!(
        s.record().await?.state,
        DriveState::Reported { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn a_step_that_ends_while_the_drive_is_paused_keeps_its_conclusion() -> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    s.drive.turn_started(THREAD).await?;
    // The Router parks the session while its step still runs.
    s.drive.pause(THREAD, "你接管了这个会话").await?;
    s.host
        .finish("completed", "分好了 3 个主题目录，索引也写了");
    s.drive.turn_stopped(THREAD).await?;
    s.drive.step(THREAD).await?;
    let record = s.record().await?;
    assert_eq!(
        record.state,
        DriveState::Paused {
            reason: format!(
                "你接管了这个会话{}",
                codex_superagent_extension::drive::DONE_WHILE_PAUSED
            )
        }
    );
    assert_eq!(
        record.report.map(|report| report.text),
        Some("分好了 3 个主题目录，索引也写了".to_owned())
    );
    assert!(
        s.notices().await?.is_empty(),
        "a pause the Router made is not told back"
    );
    // Handed back, the Router answers the conclusion the step ended with.
    s.drive
        .say(
            THREAD,
            say("把索引按日期排一遍。", "索引也写了", "按主题分好"),
        )
        .await?;
    assert_eq!(s.record().await?.steps, 2);
    Ok(())
}

#[tokio::test]
async fn a_step_said_while_a_turn_still_runs_joins_that_turn() -> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    s.drive.turn_started(THREAD).await?;
    s.drive.pause(THREAD, "你接管了这个会话").await?;
    // The step's turn still runs when the Router speaks again: the words join it, and no turn of
    // the step's own is waited for.
    s.host.finish("inProgress", "还在整理");
    let record = s
        .drive
        .say(
            THREAD,
            say("顺便把索引也写了。", "整理笔记目录", "按主题分好"),
        )
        .await?
        .expect("a record");
    assert_eq!(
        (record.steps, record.inflight, record.started, record.state),
        (2, None, Some(2), DriveState::Driving)
    );
    // That turn's end answers step 2.
    s.host.end_latest("completed", "分好了，索引也写了");
    s.drive.turn_stopped(THREAD).await?;
    s.drive.step(THREAD).await?;
    assert!(
        s.notices()
            .await?
            .last()
            .expect("a notice")
            .contains("第 2 步结束")
    );
    Ok(())
}

const ROUTER_B: &str = "019a0000-0000-7000-8000-00000000d0bb";

/// A moment, so the times the record keeps come one after the other.
async fn tick() {
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
}

#[tokio::test]
async fn notices_wait_while_the_router_is_in_a_turn_and_then_go_together() -> anyhow::Result<()> {
    let s = setup_routed(&[ROUTER]).await?;
    s.start().await?;
    s.drive
        .start(OPENED, simple(s.work.path().to_owned()))
        .await?;
    tick().await;
    s.turn("笔记分好了 3 个主题").await?;
    s.drive.turn_started(OPENED).await?;
    s.host.finish("completed", "索引写好了");
    s.drive.turn_stopped(OPENED).await?;
    s.drive.step(OPENED).await?;
    assert_eq!(s.notices().await?.len(), 2);

    // The Router is in a turn: nothing goes, and the notices keep waiting.
    s.drive.router_turn_started(ROUTER, "router-turn-1");
    assert!(s.drive.deliver_notices(ROUTER).await.is_err());
    assert!(
        s.host
            .delivered()
            .iter()
            .all(|text| !text.starts_with("[通知]"))
    );

    // Once it is idle, both go in one message.
    s.drive.router_turn_ended(ROUTER).await?;
    s.drive
        .deliver_notices(ROUTER)
        .await
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    let sent: Vec<String> = s
        .host
        .delivered()
        .into_iter()
        .filter(|text| text.starts_with("[通知]"))
        .collect();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].starts_with("[通知] 2 路有新情况"), "{}", sent[0]);
    assert!(sent[0].contains("笔记分好了 3 个主题") && sent[0].contains("索引写好了"));
    assert!(
        FakeHost::lock(&s.host.calls).contains(&format!("deliver:{ROUTER}:Queue")),
        "queued for the Router's next turn"
    );
    assert!(s.notices().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_notice_the_router_acted_on_meanwhile_is_dropped() -> anyhow::Result<()> {
    let s = setup_routed(&[ROUTER]).await?;
    s.start().await?;
    tick().await;
    s.turn("分好了 3 个主题目录").await?;
    tick().await;
    // Busy with another session, the Router saw the conclusion in its list and answered it.
    s.drive
        .say(
            THREAD,
            say("再写个索引。", "分好了 3 个主题目录", "按主题分好"),
        )
        .await?;
    s.drive
        .deliver_notices(ROUTER)
        .await
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    assert!(
        s.host
            .delivered()
            .iter()
            .all(|text| !text.starts_with("[通知]")),
        "the answered conclusion is not delivered: {:?}",
        s.host.delivered()
    );
    assert!(s.notices().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_drives_notices_go_to_the_router_that_started_it() -> anyhow::Result<()> {
    let s = setup_routed(&[ROUTER, ROUTER_B]).await?;
    let start = DriveStart {
        router: Some(ROUTER_B.to_owned()),
        ..simple(s.work.path().to_owned())
    };
    s.drive.start(THREAD, start).await?;
    tick().await;
    s.turn("分好了 3 个主题目录").await?;
    assert!(s.drive.records_of(ROUTER).await?.is_empty());
    assert_eq!(s.drive.records_of(ROUTER_B).await?.len(), 1);
    s.drive
        .deliver_notices(ROUTER)
        .await
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    assert!(
        s.host
            .delivered()
            .iter()
            .all(|text| !text.starts_with("[通知]"))
    );
    s.drive
        .deliver_notices(ROUTER_B)
        .await
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    assert!(FakeHost::lock(&s.host.calls).contains(&format!("deliver:{ROUTER_B}:Queue")));
    Ok(())
}

#[tokio::test]
async fn a_refused_step_says_where_the_engine_looked_and_another_sessions_conclusion_is_a_basis()
-> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    s.turn("分好了 3 个主题目录").await?;
    tick().await;
    s.drive
        .start(OPENED, simple(s.work.path().to_owned()))
        .await?;
    s.drive.turn_started(OPENED).await?;
    s.host
        .finish("completed", "索引脚本放在 tools/index.py，按日期排");
    s.drive.turn_stopped(OPENED).await?;
    s.drive.step(OPENED).await?;

    // Words found nowhere: the refusal says where the engine looked and how the conclusion
    // opens, and the session's line says it still waits.
    let refused = s
        .drive
        .say(
            THREAD,
            say("再写个索引。", "索引要用脚本生成", "按主题分好"),
        )
        .await;
    let refusal = format!("{:#}", refused.expect_err("not found"));
    assert!(
        refusal.contains("找过：")
            && refusal.contains("这一路最近的结论开头是「分好了 3 个主题目录」"),
        "{refusal}"
    );
    let record = s.record().await?;
    assert!(record.refused.is_some());
    assert!(
        record.line().contains("你上一句被退回"),
        "{}",
        record.line()
    );

    // Another session's conclusion: the step records which session it quotes.
    let record = s
        .drive
        .say(
            THREAD,
            say(
                "用另一路写好的索引脚本，给这里的目录也生成索引。",
                "索引脚本放在 tools/index.py",
                "按主题分好",
            ),
        )
        .await?
        .expect("a record");
    let from = record.history.last().and_then(|step| step.from.clone());
    assert_eq!(from, Some(format!("整理笔记（{OPENED}）")));
    assert_eq!(record.refused, None);
    assert!(!record.line().contains("被退回"), "{}", record.line());
    Ok(())
}

#[tokio::test]
async fn the_persons_words_to_the_router_and_standing_rules_count_as_basis() -> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    s.turn("分好了 3 个主题目录").await?;
    let told = "以后每个目录都要有一份说明文件";
    s.drive.heard(&format!("对了，{told}。")).await?;
    // What the person told the Router serves the step and joins the goal.
    let record = s
        .drive
        .say(
            THREAD,
            say(
                "给每个主题目录写一份说明文件。",
                "分好了 3 个主题目录",
                told,
            ),
        )
        .await?
        .expect("a record");
    assert_eq!(record.corrections, vec![format!("对了，{told}。")]);
    s.turn("说明文件写好了").await?;

    let fork = "分支可以推到我的 fork";
    let english = "说明文件用英文写";
    // Only what the person said becomes a standing rule.
    assert!(s.drive.remember(fork, StandingScope::All).await.is_err());
    s.drive.heard(fork).await?;
    s.drive.heard(english).await?;
    let rule = s.drive.remember(fork, StandingScope::All).await?;
    assert_eq!(rule.id, "r1");
    assert_eq!(
        s.drive.remember(fork, StandingScope::All).await?.id,
        "r1",
        "the same words for the same sessions are kept once"
    );
    let elsewhere = StandingScope::Folder {
        path: PathBuf::from("/elsewhere"),
    };
    assert_eq!(s.drive.remember(english, elsewhere).await?.id, "r2");
    // Long after, the person's words have left the Router's recent log; the rules keep them.
    for n in 0..30 {
        s.drive.heard(&format!("第 {n} 件别的事")).await?;
    }
    let refused = s
        .drive
        .say(THREAD, say("说明文件改用英文。", "说明文件写好了", english))
        .await;
    assert!(
        refused.is_err(),
        "a rule for another folder does not hold here"
    );
    s.drive
        .say(
            THREAD,
            say("把这些改动推到用户的 fork 上。", "说明文件写好了", fork),
        )
        .await?;

    assert_eq!(
        s.drive.forget("r1").await?.map(|rule| rule.text),
        Some(fork.to_owned())
    );
    assert_eq!(s.drive.forget("r1").await?, None);
    let left: Vec<String> = s
        .drive
        .standing_rules()
        .await?
        .into_iter()
        .map(|rule| rule.id)
        .collect();
    assert_eq!(left, vec!["r2".to_owned()]);
    Ok(())
}

#[tokio::test]
async fn a_stop_for_the_persons_decision_goes_on_only_with_the_persons_words() -> anyhow::Result<()>
{
    let s = setup().await?;
    s.start_anchored().await?;
    let mut drift = audit("drift", &[], &["A1", "A2"], "1");
    drift.findings = vec!["做的不是用户要的目录结构".to_owned()];
    s.judge.then(drift);
    s.turn("按年份分了目录").await?;
    let next = say("改回按主题分目录。", "按年份分了目录", "整理笔记目录");
    let refused = s.drive.say(THREAD, next.clone()).await;
    assert!(format!("{:#}", refused.expect_err("the person decides")).contains("要用户定的事"));
    // Handing back does not take the drive past the person.
    s.drive.resume(THREAD).await?;
    assert!(matches!(
        s.record().await?.state,
        DriveState::NeedsDecision { .. }
    ));
    let words = "按主题分，不要按年份";
    s.drive.heard(words).await?;
    let record = s
        .drive
        .say(
            THREAD,
            Say {
                correction: Some(words.to_owned()),
                ..next
            },
        )
        .await?
        .expect("a record");
    assert_eq!(
        (record.state, record.corrections),
        (DriveState::Driving, vec![words.to_owned()])
    );
    Ok(())
}

#[tokio::test]
async fn a_judgment_that_cannot_run_or_records_to_fix_stay_with_the_router() -> anyhow::Result<()> {
    let s = setup().await?;
    s.start_anchored().await?;
    s.judge.break_next("route-audit 没有给出 JSON");
    s.turn("分好了主题目录").await?;
    let record = s.record().await?;
    assert_eq!(
        record.state,
        DriveState::Reported {
            summary: "分好了主题目录".to_owned()
        }
    );
    assert_eq!(
        record.report.and_then(|report| report.audit),
        Some("这一步没判成（判定程序出错：route-audit 没有给出 JSON），下一步结束再判".to_owned())
    );

    s.drive
        .say(
            THREAD,
            say("把索引写上。", "分好了主题目录", "整理笔记目录"),
        )
        .await?;
    let mut invalid = audit("invalid", &[], &["A1", "A2"], "2");
    invalid.findings = vec!["证据 E3 没有登记".to_owned()];
    s.judge.then(invalid);
    s.turn("索引写好了").await?;
    assert_eq!(
        s.record().await?.state,
        DriveState::Reported {
            summary: "记录要修：证据 E3 没有登记；索引写好了".to_owned()
        }
    );
    Ok(())
}

#[tokio::test]
async fn a_step_without_progress_changed_neither_the_records_nor_the_folder() -> anyhow::Result<()>
{
    let s = setup().await?;
    s.start_anchored().await?;
    s.judge.then(audit("anchored", &[], &["A1", "A2"], "1"));
    s.turn("先看了一遍笔记").await?;
    // The records stay the same, but the worker wrote into the folder: that is progress.
    s.drive
        .say(
            THREAD,
            say("按主题建子目录。", "先看了一遍笔记", "整理笔记目录"),
        )
        .await?;
    std::fs::create_dir_all(s.work.path().join("notes/编程"))?;
    std::fs::write(s.work.path().join("notes/编程/a.md"), "x")?;
    s.turn("建好了编程目录").await?;
    assert_eq!(s.record().await?.unchanged, 0);
    tick().await;
    // Two steps that changed nothing at all stop for the person.
    for (text, conclusion) in [
        ("接着建其余的目录。", "还在想怎么分"),
        ("先列出要建的目录名。", "还没想好"),
    ] {
        let last = s.record().await?.report.expect("a report").text;
        s.drive
            .say(THREAD, say(text, &last, "整理笔记目录"))
            .await?;
        s.turn(conclusion).await?;
    }
    assert_eq!(
        s.record().await?.state,
        DriveState::NeedsDecision {
            reason: "连续 2 步既没有回填记录，文件夹里也没有任何改动（第 4 步）".to_owned()
        }
    );
    Ok(())
}

#[tokio::test]
async fn say_raises_the_cap_and_tells_the_worker_where_to_stop() -> anyhow::Result<()> {
    let s = setup().await?;
    let start = DriveStart {
        max_steps: Some(1),
        ..simple(s.work.path().to_owned())
    };
    s.drive.start(THREAD, start).await?;
    s.turn("分好了，还差索引").await?;
    assert!(matches!(s.record().await?.state, DriveState::Paused { .. }));
    let next = Say {
        until: Some("索引写好、能按日期排序为止".to_owned()),
        max_steps: Some(3),
        ..say("接着写索引。", "还差索引", "按主题分好")
    };
    let record = s.drive.say(THREAD, next).await?.expect("a record");
    assert_eq!((record.steps, record.max_steps), (2, 3));
    assert_eq!(
        s.host.delivered().last().map(String::as_str),
        Some("接着写索引。\n\n这一步做到这里停：索引写好、能按日期排序为止")
    );
    Ok(())
}

/// Minutes ago, as the record keeps times.
fn minutes_ago(minutes: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::minutes(minutes))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[tokio::test]
async fn a_session_waiting_for_another_ones_conclusion_goes_back_to_the_router_with_it()
-> anyhow::Result<()> {
    let s = setup_routed(&[ROUTER]).await?;
    s.start().await?;
    s.turn("分好了 3 个主题目录").await?;
    s.drive
        .start(OPENED, simple(s.work.path().to_owned()))
        .await?;
    let waiting = "等另一路把索引脚本写好";
    // A session at work does not park.
    let busy = s
        .drive
        .wait(OPENED, waiting, WaitOn::Session(THREAD.to_owned()), None)
        .await;
    assert!(format!("{:#}", busy.expect_err("at work")).contains("还在做"));
    let record = s
        .drive
        .wait(
            THREAD,
            waiting,
            WaitOn::Session(OPENED.to_owned()),
            Some(30),
        )
        .await?
        .expect("a record");
    assert_eq!(
        record.state,
        DriveState::Paused {
            reason: format!("等待：{waiting}")
        }
    );
    assert!(
        record.line().contains(&format!("| 等待：{waiting}")),
        "{}",
        record.line()
    );
    assert_eq!(
        codex_superagent_extension::router::drive_attention(&record),
        None,
        "parked on purpose, not waiting for the person"
    );

    s.drive.turn_started(OPENED).await?;
    s.host
        .finish("completed", "索引脚本放在 tools/index.py，按日期排");
    s.drive.turn_stopped(OPENED).await?;
    s.drive.step(OPENED).await?;
    let record = s.record().await?;
    assert_eq!(
        (record.state, record.wait),
        (
            DriveState::Reported {
                summary: format!("等的东西到了：{waiting}")
            },
            None
        )
    );
    let notices = s.notices().await?;
    assert!(
        notices.iter().any(|notice| notice
            .starts_with(&format!("推进 · 整理笔记 · 等的东西到了：{waiting}"))
            && notice.contains("索引脚本放在 tools/index.py")),
        "{notices:?}"
    );
    Ok(())
}

#[tokio::test]
async fn the_round_wakes_a_wait_for_a_file_or_a_time_and_reminds_once_of_a_long_wait()
-> anyhow::Result<()> {
    let s = setup_routed(&[ROUTER]).await?;
    s.start().await?;
    s.turn("分好了 3 个主题目录").await?;
    let file = s.work.path().join("对照结果.csv");
    s.drive
        .wait(THREAD, "等对照结果", WaitOn::File(file.clone()), Some(20))
        .await?;
    s.drive.sweep().await?;
    assert!(s.record().await?.wait.is_some(), "nothing came yet");
    // Past the reminder: told once.
    s.rewrite(THREAD, |record| {
        if let Some(wait) = record.wait.as_mut() {
            wait.remind = Some(minutes_ago(1));
        }
    })
    .await?;
    s.drive.sweep().await?;
    s.drive.sweep().await?;
    let reminders = s
        .notices()
        .await?
        .into_iter()
        .filter(|notice| notice.contains("还在等：等对照结果"))
        .count();
    assert_eq!(reminders, 1);

    std::fs::write(&file, "a,b\n")?;
    s.drive.sweep().await?;
    assert_eq!(
        s.record().await?.state,
        DriveState::Reported {
            summary: "等的东西到了：等对照结果".to_owned()
        }
    );

    s.drive
        .say(
            THREAD,
            say(
                "用对照结果核一遍目录。",
                "分好了 3 个主题目录",
                "按主题分好",
            ),
        )
        .await?;
    s.turn("核过了，没问题").await?;
    s.drive
        .wait(THREAD, "等十分钟后服务起来", WaitOn::Minutes(10), None)
        .await?;
    s.rewrite(THREAD, |record| {
        if let Some(wait) = record.wait.as_mut() {
            wait.on = codex_superagent_extension::drive::WaitFor::Time { at: minutes_ago(1) };
        }
    })
    .await?;
    s.drive.sweep().await?;
    assert_eq!(s.record().await?.wait, None);
    Ok(())
}

#[tokio::test]
async fn the_round_marks_what_it_notices_and_tells_the_router_once() -> anyhow::Result<()> {
    let s = setup_routed(&[ROUTER]).await?;
    s.start().await?;
    s.turn("分好了 3 个主题目录").await?;
    s.drive.sweep().await?;
    assert!(s.record().await?.marks.is_empty());

    // Nobody answered the conclusion for half an hour.
    s.rewrite(THREAD, |record| {
        record.changed = Some(minutes_ago(31));
        record.acted = Some(minutes_ago(40));
    })
    .await?;
    s.drive.sweep().await?;
    s.drive.sweep().await?;
    let record = s.record().await?;
    assert_eq!(record.marks, vec!["交了结论半小时以上没人回".to_owned()]);
    assert!(
        record.line().ends_with("| 注意：交了结论半小时以上没人回"),
        "{}",
        record.line()
    );
    let told: Vec<String> = s
        .notices()
        .await?
        .into_iter()
        .filter(|notice| notice.contains("注意："))
        .collect();
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(
        told[0].contains("在等别的事就用 wait 写明等什么"),
        "{}",
        told[0]
    );

    // Answered: the mark goes with the state it was about.
    s.drive
        .say(
            THREAD,
            say("再写个索引。", "分好了 3 个主题目录", "按主题分好"),
        )
        .await?;
    assert!(s.record().await?.marks.is_empty());
    Ok(())
}

#[tokio::test]
async fn small_steps_and_the_same_conclusion_twice_show_in_the_line() -> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    let same = "目录还在整理中，已经处理了一部分笔记，剩下的部分下一步继续处理完毕再报告";
    s.turn(&format!("{same}（一）")).await?;
    for n in 2..=6 {
        let last = s.record().await?.report.expect("a report").text;
        s.drive
            .say(
                THREAD,
                say(&format!("接着整理第 {n} 批。"), &last, "按主题分好"),
            )
            .await?;
        s.turn(&format!("{same}（{n}）")).await?;
    }
    s.drive.sweep().await?;
    assert_eq!(
        s.record().await?.marks,
        vec![
            "最近两次结论开头一样，可能在原地打转".to_owned(),
            "连着 5 步都不到 3 分钟：一步交代一个完整的小目标，说清做到哪停".to_owned(),
        ]
    );
    assert!(
        s.notices()
            .await?
            .iter()
            .all(|notice| !notice.contains("注意：")),
        "marks the Router reads in its list are not told"
    );
    Ok(())
}

#[tokio::test]
async fn every_change_of_a_drive_is_one_line_of_its_events() -> anyhow::Result<()> {
    let s = setup_routed(&[ROUTER]).await?;
    s.start().await?;
    s.turn("分好了 3 个主题目录").await?;
    s.drive
        .say(
            THREAD,
            say("再写个索引。", "分好了 3 个主题目录", "按主题分好"),
        )
        .await?;
    s.drive.stop(THREAD).await?;
    let events: Vec<serde_json::Value> = std::fs::read_to_string(&s.events)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let seen: Vec<(String, u64)> = events
        .iter()
        .map(|event| {
            (
                event["event"].as_str().unwrap_or_default().to_owned(),
                event["drive"]["step"].as_u64().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        vec![
            ("started".to_owned(), 1),
            ("reported".to_owned(), 1),
            ("driving".to_owned(), 2),
            ("released".to_owned(), 2),
        ]
    );
    assert_eq!(
        (
            events[1]["drive"]["thread"].clone(),
            events[1]["drive"]["detail"].clone()
        ),
        (json!(THREAD), json!("分好了 3 个主题目录"))
    );
    Ok(())
}

#[tokio::test]
async fn a_router_adopts_a_drive_and_its_notices_follow() -> anyhow::Result<()> {
    let s = setup_routed(&[ROUTER, ROUTER_B]).await?;
    let start = DriveStart {
        router: Some(ROUTER.to_owned()),
        ..simple(s.work.path().to_owned())
    };
    s.drive.start(THREAD, start).await?;
    assert!(
        s.drive
            .adopt(THREAD, "019a0000-0000-7000-8000-00000000dfff")
            .await
            .is_err(),
        "only a registered Router adopts"
    );
    s.drive.adopt(THREAD, ROUTER_B).await?;
    tick().await;
    s.turn("分好了 3 个主题目录").await?;
    assert!(s.drive.records_of(ROUTER).await?.is_empty());
    assert_eq!(s.drive.records_of(ROUTER_B).await?.len(), 1);
    s.drive
        .deliver_notices(ROUTER_B)
        .await
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    assert!(FakeHost::lock(&s.host.calls).contains(&format!("deliver:{ROUTER_B}:Queue")));
    Ok(())
}
