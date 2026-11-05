//! The drive loop: each judgment either sends one step written from the anchor and the audit, or
//! stops and says why; a step in flight is never sent twice, not even across a restart.

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
use codex_superagent_extension::drive::Audit;
use codex_superagent_extension::drive::DriveRecord;
use codex_superagent_extension::drive::DriveService;
use codex_superagent_extension::drive::DriveState;
use codex_superagent_extension::drive::Judge;
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

#[derive(Default)]
struct FakeHost {
    calls: Mutex<Vec<String>>,
    delivered: Mutex<Vec<String>>,
    queue: Mutex<Vec<(String, String)>>,
    latest: Mutex<Option<String>>,
}

impl FakeHost {
    fn lock<T>(cell: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        cell.lock().unwrap_or_else(PoisonError::into_inner)
    }
    fn delivered(&self) -> Vec<String> {
        Self::lock(&self.delivered).clone()
    }
    fn set_latest(&self, status: &str) {
        *Self::lock(&self.latest) = Some(status.to_owned());
    }
}

impl SuperagentHost for FakeHost {
    fn start_thread(&self, _spec: ThreadSpec) -> BoxFut<'_, anyhow::Result<ThreadId>> {
        Box::pin(async { anyhow::bail!("the drive loop starts no thread") })
    }
    fn bind_profile(&self, _thread: ThreadId, _profile: String) -> BoxFut<'_, anyhow::Result<()>> {
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
        Box::pin(async { Ok(()) })
    }
    fn session_turns(
        &self,
        _thread: ThreadId,
        _limit: u32,
    ) -> BoxFut<'_, anyhow::Result<Vec<TurnBrief>>> {
        let latest = Self::lock(&self.latest).clone();
        Box::pin(async move {
            Ok(latest
                .map(|status| TurnBrief {
                    id: "turn-1".to_owned(),
                    status,
                    user: Vec::new(),
                    assistant: Vec::new(),
                })
                .into_iter()
                .collect())
        })
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

struct Setup {
    _dir: TempDir,
    _work: TempDir,
    home: PathBuf,
    host: Arc<FakeHost>,
    judge: Arc<ScriptedJudge>,
    effects: Effects,
    drive: DriveService,
}

/// The effect queue is not started: each test runs the judgments itself, in order.
async fn setup() -> anyhow::Result<Setup> {
    let (dir, store) = open_store().await?;
    let work = TempDir::new()?;
    let home = workspace(work.path())?;
    let host = Arc::new(FakeHost::default());
    let judge = Arc::new(ScriptedJudge::default());
    let effects =
        Effects::new(store.clone(), HealthBoard::new(), FakeClock::at(T0).clock()).await?;
    let drive = DriveService::install(
        store,
        effects.clone(),
        host.clone() as Arc<dyn SuperagentHost>,
        judge.clone() as Arc<dyn Judge>,
    )
    .await?;
    Ok(Setup {
        _dir: dir,
        _work: work,
        home,
        host,
        judge,
        effects,
        drive,
    })
}

impl Setup {
    async fn start(&self) -> anyhow::Result<DriveRecord> {
        self.drive
            .start(
                THREAD,
                self.home.clone(),
                "整理笔记".to_owned(),
                /*max_steps*/ None,
            )
            .await
    }
    async fn record(&self) -> anyhow::Result<DriveRecord> {
        Ok(self.drive.record(THREAD).await?.expect("a drive record"))
    }
    /// The step's turn starts and ends normally.
    async fn turn(&self) -> anyhow::Result<()> {
        self.drive.turn_started(THREAD).await?;
        self.host.set_latest("completed");
        self.drive.turn_stopped(THREAD).await
    }
}

#[tokio::test]
async fn a_step_names_the_item_a_correction_affects_and_is_recorded_before_it_is_sent()
-> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    assert_eq!(s.effects.queued("drive-step").await?.len(), 1);
    s.judge.then(audit("anchored", &[], &["A1", "A2"], "1"));
    s.drive.step(THREAD).await?;

    let delivered = s.host.delivered();
    assert_eq!(delivered.len(), 1);
    let prompt = &delivered[0];
    assert!(prompt.starts_with("[推进 第1步 · 整理笔记]\n"), "{prompt}");
    assert!(
        prompt.contains("本步推进验收项 A2：根目录有索引文件（当前 open）"),
        "{prompt}"
    );
    assert!(prompt.contains("其余未闭合：A1；已核实：无"), "{prompt}");
    assert!(prompt.contains("回填到 route-anchor"), "{prompt}");
    assert_eq!(
        FakeHost::lock(&s.host.calls).clone(),
        vec![
            format!("resume:{THREAD}:None"),
            format!("deliver:{THREAD}:Queue")
        ]
    );
    let record = s.record().await?;
    assert_eq!(
        (
            record.steps,
            record.inflight,
            record.history[0].focus.as_str()
        ),
        (1, Some(1), "A2")
    );
    assert!(s.drive.drives(THREAD).await?);

    // The step is in flight: judging again sends nothing; nor while its turn runs.
    s.drive.step(THREAD).await?;
    s.drive.turn_started(THREAD).await?;
    s.host.set_latest("inProgress");
    s.drive.step(THREAD).await?;
    assert_eq!(s.host.delivered().len(), 1);
    Ok(())
}

#[tokio::test]
async fn two_steps_without_a_back_filled_stage_stop_the_drive() -> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    s.judge.then(audit("anchored", &[], &["A1"], "same"));
    s.drive.step(THREAD).await?;
    for _ in 0..2 {
        s.turn().await?;
        s.drive.step(THREAD).await?;
    }
    assert_eq!(s.host.delivered().len(), 2);
    let record = s.record().await?;
    assert_eq!(
        (record.state, record.unchanged),
        (
            DriveState::NeedsDecision {
                reason: "连续 2 步 Project 和锚定记录都没有变化：worker 没有回填阶段".to_owned()
            },
            2
        )
    );
    assert!(!s.drive.drives(THREAD).await?);
    Ok(())
}

#[tokio::test]
async fn a_verified_delivery_or_a_drift_stops_without_a_step() -> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    s.judge.then(audit("anchored", &["A1"], &["A2"], "1"));
    s.drive.step(THREAD).await?;
    s.turn().await?;
    s.judge.then(audit("anchored", &["A1", "A2"], &[], "2"));
    s.drive.step(THREAD).await?;
    assert_eq!(s.host.delivered().len(), 1);
    assert_eq!(
        s.record().await?.state,
        DriveState::Deliverable {
            summary: "anchored；已核实 A1、A2；未闭合 无；不一致 无".to_owned()
        }
    );

    s.drive.resume(THREAD).await?;
    let mut drift = audit("drift", &[], &["A2"], "3");
    drift.findings = vec!["code=no_progress_streak stage=S02".to_owned()];
    s.judge.then(drift);
    s.drive.step(THREAD).await?;
    assert_eq!(
        s.record().await?.state,
        DriveState::NeedsDecision {
            reason: "偏移：code=no_progress_streak stage=S02".to_owned()
        }
    );
    assert_eq!(s.host.delivered().len(), 1);
    Ok(())
}

#[tokio::test]
async fn the_persons_words_reach_the_next_step_once() -> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    s.judge.then(audit("anchored", &[], &["A1"], "1"));
    s.drive.step(THREAD).await?;
    s.drive.person_input(THREAD, "子目录名用英文").await?;
    s.turn().await?;
    s.judge.then(audit("anchored", &[], &["A1"], "2"));
    s.drive.step(THREAD).await?;
    let delivered = s.host.delivered();
    assert!(
        delivered[1].contains("- 子目录名用英文\n"),
        "{}",
        delivered[1]
    );
    assert_eq!(s.record().await?.notes, Vec::<String>::new());
    Ok(())
}

#[tokio::test]
async fn an_interruption_or_a_failed_step_pauses_and_resume_drives_again() -> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    s.judge.then(audit("anchored", &[], &["A1"], "1"));
    s.drive.step(THREAD).await?;
    s.drive.turn_started(THREAD).await?;
    s.drive.turn_interrupted(THREAD).await?;
    assert!(matches!(s.record().await?.state, DriveState::Paused { .. }));
    assert!(!s.drive.drives(THREAD).await?);

    s.drive.resume(THREAD).await?;
    assert!(s.drive.drives(THREAD).await?);
    s.host.set_latest("failed");
    s.drive.step(THREAD).await?;
    assert_eq!(
        s.record().await?.state,
        DriveState::Paused {
            reason: "第 1 步没有正常结束（failed）".to_owned()
        }
    );
    assert_eq!(s.host.delivered().len(), 1);
    Ok(())
}

#[tokio::test]
async fn stop_and_recovery_withdraw_only_the_engines_queued_steps() -> anyhow::Result<()> {
    let s = setup().await?;
    s.start().await?;
    s.judge.then(audit("anchored", &[], &["A1"], "1"));
    s.drive.step(THREAD).await?;
    *FakeHost::lock(&s.host.queue) = vec![
        ("q1".to_owned(), s.host.delivered()[0].clone()),
        ("q2".to_owned(), "我自己排的消息".to_owned()),
    ];

    // A restart before the step's turn started: no replay, the queued step goes.
    s.drive.recover().await?;
    assert_eq!(
        s.record().await?.state,
        DriveState::Paused {
            reason: "服务重启时第 1 步已交出但没有看到开始，没有自动重放".to_owned()
        }
    );
    assert_eq!(
        FakeHost::lock(&s.host.queue).clone(),
        vec![("q2".to_owned(), "我自己排的消息".to_owned())]
    );

    FakeHost::lock(&s.host.queue).push(("q3".to_owned(), "[推进 第9步 · 旧]".to_owned()));
    let released = s.drive.stop(THREAD).await?;
    assert!(released.is_some());
    assert_eq!(s.drive.record(THREAD).await?, None);
    assert_eq!(
        FakeHost::lock(&s.host.queue).clone(),
        vec![("q2".to_owned(), "我自己排的消息".to_owned())]
    );
    Ok(())
}

#[tokio::test]
async fn start_refuses_a_project_without_a_committed_anchor_and_a_second_start()
-> anyhow::Result<()> {
    let s = setup().await?;
    let bare = TempDir::new()?;
    let refused = s
        .drive
        .start(
            THREAD,
            bare.path().to_path_buf(),
            "x".to_owned(),
            /*max_steps*/ None,
        )
        .await;
    assert!(refused.is_err());
    s.start().await?;
    assert!(s.start().await.is_err());
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
