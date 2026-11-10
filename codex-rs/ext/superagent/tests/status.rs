//! The status interface: exit codes and the JSON the watchdog reads (design §9, T-18).

mod common;

use std::sync::Arc;

use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_superagent_extension::Health;
use codex_superagent_extension::SuperagentService;
use codex_superagent_extension::core::Assignment;
use codex_superagent_extension::core::AssignmentState;
use codex_superagent_extension::core::Attention;
use codex_superagent_extension::core::ChannelRef;
use codex_superagent_extension::core::DecisionKind;
use codex_superagent_extension::core::Execution;
use codex_superagent_extension::core::LastActivity;
use codex_superagent_extension::core::LastActivityKind;
use codex_superagent_extension::core::Report;
use codex_superagent_extension::core::ReportState;
use codex_superagent_extension::status::STALL_MS;
use codex_superagent_extension::status::StatusPaths;
use codex_superagent_extension::status::exit_code;
use codex_superagent_extension::status::render_one_line;
use codex_superagent_extension::status::status_with;
use codex_utils_absolute_path::test_support::PathExt;
use common::ticket;
use common::trigger;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

/// 2026-01-01T00:00:00Z in milliseconds.
const T1: i64 = 1_767_225_600_000;

async fn service() -> anyhow::Result<(TempDir, Arc<SuperagentService>, StatusPaths)> {
    let dir = TempDir::new()?;
    let sqlite = SqliteConfig::new_for_testing(dir.path().abs());
    let state = StateRuntime::init(sqlite, "test-provider".to_owned()).await?;
    let service = Arc::new(SuperagentService::new(state, dir.path().join("superagent")));
    let paths = StatusPaths::for_home(dir.path());
    Ok((dir, service, paths))
}

#[tokio::test]
async fn a_second_process_on_the_same_home_does_not_run_the_engine() -> anyhow::Result<()> {
    let (dir, service, _paths) = service().await?;
    service.start().await?;
    // Another process opens the same CODEX_HOME (an embedded app-server, a daemon starting while
    // the old one drains): the engine lock is taken, so it runs nothing on the ledger.
    let sqlite = SqliteConfig::new_for_testing(dir.path().abs());
    let state = StateRuntime::init(sqlite, "test-provider".to_owned()).await?;
    let second = Arc::new(SuperagentService::new(state, dir.path().join("superagent")));
    let refused = match second.start().await {
        Ok(_) => panic!("the second engine must not start"),
        Err(err) => format!("{err:#}"),
    };
    assert!(refused.contains("another process"), "{refused}");
    assert!(second.runtime().is_none());
    assert!(dir.path().join("superagent").join("engine.lock").is_file());
    Ok(())
}

#[tokio::test]
async fn waits_for_configuration_until_the_engine_runs_with_a_pm() -> anyhow::Result<()> {
    let (_dir, service, paths) = service().await?;
    let before = status_with(&service, &paths, T1).await?;
    assert!(!before.started);
    assert_eq!(exit_code(&before), 3);
    assert!(render_one_line(&before).contains("引擎没有启动"));
    service.start().await?;
    let no_pm = status_with(&service, &paths, T1).await?;
    assert!(no_pm.started);
    assert_eq!(exit_code(&no_pm), 3);
    service.pm_threads().register("pm-thread-1")?;
    let ready = status_with(&service, &paths, T1).await?;
    assert_eq!((exit_code(&ready), ready.exit_code), (0, 0));
    assert_eq!(ready.pm_threads, vec!["pm-thread-1"]);
    assert_eq!(render_one_line(&ready), "superagent 正常：0 张在途工单");
    let json = serde_json::to_value(&ready)?;
    for field in [
        "at",
        "started",
        "pmThreads",
        "health",
        "effects",
        "tickets",
        "pmGroups",
        "attention",
        "hookErrors",
        "watchdogHeartbeat",
        "exitCode",
    ] {
        assert!(json.get(field).is_some(), "missing {field}");
    }
    assert_eq!(json["health"]["runtime"]["state"], "ready");
    assert_eq!(json["health"]["models"]["detail"], "not tracked in A1");
    Ok(())
}

#[tokio::test]
async fn a_part_in_error_answers_two() -> anyhow::Result<()> {
    let (_dir, service, paths) = service().await?;
    service.start().await?;
    service.pm_threads().register("pm-thread-1")?;
    let entry = service.board().claim(
        "relay",
        Some(Health::error(
            "1 waiting since x; the last failure: offline",
            None,
        )),
    )?;
    let status = status_with(&service, &paths, T1).await?;
    assert_eq!(status.exit_code, 2);
    assert!(render_one_line(&status).contains("relay"));
    entry.release();
    let status = status_with(&service, &paths, T1).await?;
    assert_eq!(status.exit_code, 0);
    Ok(())
}

#[tokio::test]
async fn a_stalled_ticket_answers_two_but_one_waiting_for_a_person_does_not() -> anyhow::Result<()>
{
    let (_dir, service, paths) = service().await?;
    service.start().await?;
    service.pm_threads().register("pm-thread-1")?;
    let runtime = service
        .runtime()
        .ok_or_else(|| anyhow::anyhow!("runtime"))?;
    let reported = |id: &str, state: ReportState| {
        let mut t = ticket(id, false, &[AssignmentState::Running], &[]);
        t.title = format!("{id} title");
        t.execution = Some(Execution {
            name: format!("t001_{id}"),
            directory: "/tmp/x".into(),
            assignment: String::new(),
        });
        let assignment: &mut Assignment = &mut t.assignments[0];
        assignment.report = Some(Report {
            state,
            summary: "x".repeat(300),
            at: "2026-01-01T00:00:00.000Z".into(),
        });
        assignment.executor_ref = Some("thread-1".into());
        assignment.environment = Some("local".into());
        t
    };
    let mut asking = reported("asking", ReportState::Blocked);
    asking.attention = Some(Attention {
        id: "q1".into(),
        question: "用哪个分支？".into(),
        trigger_id: None,
    });
    let working = reported("working", ReportState::Progress);
    runtime
        .store
        .atomic(async |tx| {
            tx.save_ticket(&working).await?;
            tx.save_ticket(&asking).await
        })
        .await?;
    let fresh = status_with(&service, &paths, T1 + 60 * 60 * 1000).await?;
    assert_eq!(fresh.exit_code, 0);
    assert_eq!(fresh.tickets.len(), 2);
    let stale = status_with(&service, &paths, T1 + STALL_MS + 1).await?;
    assert_eq!(stale.exit_code, 2);
    let by_id = |id: &str| {
        stale
            .tickets
            .iter()
            .find(|t| t.id == id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("ticket {id}"))
    };
    let working = by_id("working")?;
    assert!(working.stalled);
    assert_eq!(working.stage, "working");
    assert_eq!(working.name.as_deref(), Some("t001_working"));
    assert_eq!(working.thread_id.as_deref(), Some("thread-1"));
    assert_eq!(working.environment.as_deref(), Some("local"));
    assert_eq!(
        working.report.as_ref().map(|r| r.summary.chars().count()),
        Some(161)
    );
    let asking = by_id("asking")?;
    assert!(!asking.stalled);
    assert!(asking.waits_for_person);
    assert_eq!(asking.stage, "person/answer");
    assert_eq!(stale.attention.len(), 1);
    assert_eq!(stale.attention[0].question, "用哪个分支？");
    assert!(render_one_line(&stale).contains("t001_working"));
    Ok(())
}

#[tokio::test]
async fn exposes_last_activity_and_minutes_since_it() -> anyhow::Result<()> {
    let (_dir, service, paths) = service().await?;
    service.start().await?;
    service.pm_threads().register("pm-thread-1")?;
    let runtime = service
        .runtime()
        .ok_or_else(|| anyhow::anyhow!("runtime"))?;
    let mut ticket = ticket("activity", false, &[AssignmentState::Running], &[]);
    ticket.last_activity = Some(LastActivity {
        at: "2026-01-01T00:00:00.000Z".into(),
        kind: LastActivityKind::Report,
        text: "修复完成".into(),
    });
    runtime
        .store
        .atomic(async |tx| tx.save_ticket(&ticket).await)
        .await?;

    let status = status_with(&service, &paths, T1 + 2 * 60 * 60 * 1000).await?;
    let activity = status
        .tickets
        .first()
        .ok_or_else(|| anyhow::anyhow!("ticket"))?;
    assert_eq!(activity.last_activity_minutes, Some(120));
    assert_eq!(
        activity.last_activity.as_ref(),
        ticket.last_activity.as_ref()
    );
    let json = serde_json::to_value(activity)?;
    assert_eq!(json["lastActivity"]["kind"], "report");
    assert_eq!(json["lastActivityMinutes"], 120);
    Ok(())
}

#[tokio::test]
async fn reads_the_orgs_files_when_they_exist() -> anyhow::Result<()> {
    let (dir, service, paths) = service().await?;
    service.start().await?;
    service.pm_threads().register("pm-thread-1")?;
    let none = status_with(&service, &paths, T1).await?;
    assert_eq!((none.hook_errors, none.watchdog_heartbeat), (None, None));
    std::fs::create_dir_all(dir.path().join("org/pm-codext/state"))?;
    std::fs::write(&paths.hook_errors, "one\ntwo\n\n")?;
    std::fs::write(
        &paths.watchdog,
        r#"{"t": "2026-11-02T10:00:00+08:00", "ok": true, "problems": []}"#,
    )?;
    let some = status_with(&service, &paths, T1).await?;
    assert_eq!(some.hook_errors, Some(2));
    assert_eq!(
        some.watchdog_heartbeat.as_deref(),
        Some("2026-11-02T10:00:00+08:00")
    );
    Ok(())
}

#[tokio::test]
async fn groups_open_tickets_and_recent_decisions_by_pm_project() -> anyhow::Result<()> {
    let (_dir, service, paths) = service().await?;
    service.start().await?;
    std::fs::create_dir_all(
        service
            .pm_threads()
            .file()
            .parent()
            .ok_or_else(|| anyhow::anyhow!("pm threads parent"))?,
    )?;
    std::fs::write(
        service.pm_threads().file(),
        "pm-main project=codex\npm-fem project=fem\npm-default\n",
    )?;

    let runtime = service
        .runtime()
        .ok_or_else(|| anyhow::anyhow!("runtime"))?;
    let channel = |thread: &str| ChannelRef {
        source: "codext".into(),
        thread: thread.into(),
        address: Default::default(),
    };
    let mut main_ticket = ticket("ticket-main", false, &[AssignmentState::Running], &[]);
    main_ticket.decision_id = "decision-main-1".into();
    main_ticket.channels = vec![channel("pm-main")];
    main_ticket.execution = Some(Execution {
        name: "t001_main".into(),
        ..Execution::default()
    });
    let mut fem_ticket = ticket("ticket-fem", false, &[AssignmentState::Running], &[]);
    fem_ticket.decision_id = "decision-fem".into();
    fem_ticket.channels = vec![channel("pm-fem")];
    fem_ticket.execution = Some(Execution {
        name: "t002_fem".into(),
        ..Execution::default()
    });

    let trigger_for = |id: &str, pm: &str| {
        let mut trigger = trigger(id, "codext", &[("codext", "request")], id, id);
        trigger.channel = Some(channel(pm));
        trigger
    };
    let decision_for = |id: &str, trigger_id: &str, ticket_id: Option<&str>| {
        let mut decision = common::decision(id, "codext:pm/request");
        decision.kind = DecisionKind::Accept;
        decision.trigger_ids = vec![trigger_id.into()];
        decision.ticket_id = ticket_id.map(str::to_owned);
        decision
    };
    let triggers = [
        trigger_for("trigger-main-1", "pm-main"),
        trigger_for("trigger-fem", "pm-fem"),
        trigger_for("trigger-main-2", "pm-main"),
        trigger_for("trigger-main-3", "pm-main"),
    ];
    let decisions = [
        decision_for("decision-main-1", "trigger-main-1", Some("ticket-main")),
        decision_for("decision-fem", "trigger-fem", Some("ticket-fem")),
        decision_for("decision-main-2", "trigger-main-2", None),
        decision_for("decision-main-3", "trigger-main-3", None),
    ];
    runtime
        .store
        .atomic(async |tx| {
            for trigger in &triggers {
                tx.save_trigger(trigger).await?;
            }
            for decision in &decisions {
                tx.save_decision(decision).await?;
            }
            tx.save_ticket(&main_ticket).await?;
            tx.save_ticket(&fem_ticket).await
        })
        .await?;

    let status = status_with(&service, &paths, T1).await?;
    assert_eq!(status.pm_groups.len(), 3);
    let main = &status.pm_groups[0];
    assert_eq!(main.pm_thread, "pm-main");
    assert_eq!(main.project, "codex");
    assert_eq!(main.ticket_count, 1);
    assert_eq!(main.ticket_names, vec!["t001_main"]);
    assert_eq!(
        main.recent_decisions
            .iter()
            .map(|decision| decision.id.as_str())
            .collect::<Vec<_>>(),
        vec!["decision-main-3", "decision-main-2", "decision-main-1"]
    );
    let fem = &status.pm_groups[1];
    assert_eq!(fem.project, "fem");
    assert_eq!(fem.ticket_count, 1);
    assert_eq!(fem.ticket_names, vec!["t002_fem"]);
    assert_eq!(
        fem.recent_decisions
            .iter()
            .map(|decision| decision.id.as_str())
            .collect::<Vec<_>>(),
        vec!["decision-fem"]
    );
    assert_eq!(status.pm_groups[2].project, "default");
    assert_eq!(status.pm_groups[2].ticket_count, 0);
    let json = serde_json::to_value(&status)?;
    assert_eq!(json["pmGroups"][0]["pmThread"], "pm-main");
    assert_eq!(json["pmGroups"][0]["ticketCount"], 1);
    assert_eq!(
        json["pmGroups"][0]["ticketNames"],
        serde_json::json!(["t001_main"])
    );
    assert_eq!(
        json["pmGroups"][0]["recentDecisions"]
            .as_array()
            .map(Vec::len),
        Some(3)
    );
    Ok(())
}
