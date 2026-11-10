//! The status interface (design §9): what the external watchdog and the Mac launcher read, and
//! the exit code the watchdog acts on.
//!
//! The watchdog calls the app-server method `superagent/status` (JSON-RPC over the org's unix
//! socket, no params; it is experimental, so the connection must opt into the experimental API).
//! The response carries `exitCode` and this status as JSON. Exit codes, as the design's table:
//! 0 normal; 2 a part is in error (an effect kind failed three times in a row) or a ticket is
//! stalled (no effective progress for `STALL_MS` while not waiting for a person); 3 waiting for
//! configuration (the engine has not started, or no PM thread is registered). 1, the service
//! being down, is what the watchdog observes when the socket does not answer.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use chrono::DateTime;
use chrono::SecondsFormat;
use chrono::Utc;
use codex_protocol::ThreadId;
use serde::Serialize;
use sqlx::Row;

use crate::SuperagentService;
use crate::core::Decision;
use crate::core::LastActivity;
use crate::core::ReportState;
use crate::core::Ticket;
use crate::core::Trigger;
use crate::core::latest_assignment;
use crate::core::latest_report;
use crate::core::ticket_stage;
use crate::core::waits_for_person;
use crate::health::Health;
use crate::health::HealthState;

/// How long an open ticket may go without effective progress before it counts as stalled.
pub const STALL_MS: i64 = 3 * 60 * 60 * 1000;
/// The summary preview, as the cards cut it.
const PREVIEW_CHARS: usize = 160;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// When this status was read.
    pub at: String,
    /// The engine's database is open and its parts run.
    pub started: bool,
    /// The registered PM thread ids, the first being the primary PM.
    pub pm_threads: Vec<String>,
    /// The Router's level: `observe` or `drive` (`router.rs`).
    pub router_mode: &'static str,
    /// Every part's health by name: `runtime`, each effect kind, `models` and whatever else claims one.
    pub health: BTreeMap<String, Health>,
    /// Per effect kind: rows waiting, and rows that failed at least once.
    pub effects: BTreeMap<String, EffectCounts>,
    /// Every open ticket.
    pub tickets: Vec<TicketStatus>,
    /// Open tickets and recent decisions grouped by their PM thread and project.
    pub pm_groups: Vec<PmGroup>,
    /// The questions waiting for a person.
    pub attention: Vec<AttentionStatus>,
    /// Lines in the managed hooks' error log, when the file exists.
    pub hook_errors: Option<u64>,
    /// The external watchdog's last run (`t` of its state file), when the file exists.
    pub watchdog_heartbeat: Option<String>,
    /// The account moves the drive made (the Router's and the driven sessions'), oldest first.
    pub account_moves: Vec<crate::drive::AccountMove>,
    /// The usage-limit cooldowns the drive ended early because a fresh read showed the quota
    /// back, oldest first.
    pub ended_cooldowns: Vec<crate::drive::EndedCooldown>,
    /// Every driven session, in the shape of the drives' events.
    pub drives: Vec<crate::drive::DriveStatus>,
    /// The exit code the watchdog acts on (`exit_code`).
    pub exit_code: i32,
}

/// The status of one registered PM thread.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PmGroup {
    /// The Codex thread id registered as the PM.
    pub pm_thread: String,
    /// The project name from the PM thread list, or `default` when it is not named.
    pub project: String,
    /// Number of open tickets owned by this PM.
    pub ticket_count: usize,
    /// Display names of the open tickets owned by this PM.
    pub ticket_names: Vec<String>,
    /// The three newest decisions recorded for this PM, newest first.
    pub recent_decisions: Vec<Decision>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct EffectCounts {
    pub waiting: i64,
    pub failing: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TicketStatus {
    pub id: String,
    /// The display name (`tNNN_short_name`), once the executor named it.
    pub name: Option<String>,
    pub title: String,
    /// The stage card, such as `working`, `person/review` or `idle`.
    pub stage: String,
    pub waits_for_person: bool,
    /// The latest of: the last report, the last delivery to the worker, the end of its last turn;
    /// the ticket's creation when none happened yet.
    pub last_progress_at: Option<String>,
    /// No effective progress for `STALL_MS` while not waiting for a person.
    pub stalled: bool,
    /// The worker's latest turn or report activity.
    pub last_activity: Option<LastActivity>,
    /// Whole minutes since `last_activity.at`.
    pub last_activity_minutes: Option<i64>,
    pub report: Option<ReportStatus>,
    /// The worker's thread id (`assignment.executor_ref`).
    pub thread_id: Option<String>,
    /// Where its commands run: `local`, or an exec-server environment id.
    pub environment: Option<String>,
    /// The worker's Goal status, when the host can read it.
    pub goal: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReportStatus {
    pub state: ReportState,
    /// The first 160 characters of the summary.
    pub summary: String,
    pub at: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttentionStatus {
    pub ticket_id: String,
    pub name: Option<String>,
    pub question: String,
}

/// Where the A0 org keeps what the status reads from files.
#[derive(Clone, Debug)]
pub struct StatusPaths {
    pub hook_errors: PathBuf,
    pub watchdog: PathBuf,
}

impl StatusPaths {
    /// The state files under the organization's root (`executor_root`).
    pub fn for_home(home: &Path) -> Self {
        Self {
            hook_errors: home.join("org/pm-codext/state/hooks-errors.log"),
            watchdog: home.join("org/pm-codext/state/watchdog.json"),
        }
    }
}

/// The status now, with the org's files in their usual place.
pub async fn status(service: &SuperagentService) -> anyhow::Result<Status> {
    status_with(
        service,
        &StatusPaths::for_home(&crate::executor_root(service.dir())),
        Utc::now().timestamp_millis(),
    )
    .await
}

/// The status as of `now_ms`, reading the org's files at `paths`.
pub async fn status_with(
    service: &SuperagentService,
    paths: &StatusPaths,
    now_ms: i64,
) -> anyhow::Result<Status> {
    let pm_threads = service.pm_threads().list();
    let mut health = service.health();
    // Model retries are not tracked in A1; say so instead of leaving the entry out.
    health
        .entry("models".to_owned())
        .or_insert_with(|| Health::ready("not tracked in A1", None));
    let runtime = service.runtime();
    let mut effects = BTreeMap::new();
    let mut account_moves = Vec::new();
    let mut ended_cooldowns = Vec::new();
    let mut drives = Vec::new();
    let mut tickets = Vec::new();
    let mut attention = Vec::new();
    let mut pm_groups = pm_threads
        .iter()
        .map(|pm_thread| pm_group(service, pm_thread))
        .collect();
    if let Some(runtime) = &runtime {
        let rows = sqlx::query(
            "SELECT kind, COUNT(*) AS waiting, SUM(CASE WHEN attempts > 0 THEN 1 ELSE 0 END) AS failing
             FROM effect GROUP BY kind",
        )
        .fetch_all(runtime.store.pool())
        .await?;
        for row in rows {
            let kind: String = row.try_get("kind")?;
            effects.insert(
                kind,
                EffectCounts {
                    waiting: row.try_get::<i64, _>("waiting")?,
                    failing: row.try_get::<Option<i64>, _>("failing")?.unwrap_or(0),
                },
            );
        }
        if let Some(drive) = &runtime.drive {
            account_moves = drive.account_moves().await?;
            ended_cooldowns = drive.ended_cooldowns().await?;
            drives = drive
                .records()
                .await?
                .iter()
                .map(crate::drive::DriveStatus::of)
                .collect();
        }
        let host = service.host();
        let open_ticket_rows = runtime.store.tickets_open().await?;
        for ticket in &open_ticket_rows {
            let mut status = ticket_status(ticket, now_ms);
            if let (Some(host), Some(thread_id)) = (&host, &status.thread_id)
                && let Ok(thread_id) = ThreadId::from_string(thread_id)
                && let Ok(Some(goal)) = host.goal_status(thread_id).await
            {
                status.goal = Some(format!("{goal:?}"));
            }
            if let Some(question) = &ticket.attention {
                attention.push(AttentionStatus {
                    ticket_id: ticket.id.clone(),
                    name: status.name.clone(),
                    question: question.question.clone(),
                });
            }
            tickets.push(status);
        }
        pm_groups = grouped_pm_status(
            service,
            &runtime.store,
            &pm_threads,
            &open_ticket_rows,
            &tickets,
        )
        .await?;
    }
    let hook_errors = std::fs::read_to_string(&paths.hook_errors)
        .ok()
        .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count() as u64);
    let watchdog_heartbeat = std::fs::read_to_string(&paths.watchdog)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.get("t").and_then(|t| t.as_str()).map(str::to_owned));
    let mut status = Status {
        at: iso(now_ms),
        started: runtime.is_some(),
        pm_threads,
        router_mode: service.router_mode().name(),
        health,
        effects,
        tickets,
        pm_groups,
        attention,
        hook_errors,
        watchdog_heartbeat,
        account_moves,
        ended_cooldowns,
        drives,
        exit_code: 0,
    };
    status.exit_code = exit_code(&status);
    Ok(status)
}

fn pm_group(service: &SuperagentService, pm_thread: &str) -> PmGroup {
    PmGroup {
        pm_thread: pm_thread.to_owned(),
        project: service
            .pm_threads()
            .project_of(pm_thread)
            .and_then(|project| project.name)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "default".to_owned()),
        ticket_count: 0,
        ticket_names: Vec::new(),
        recent_decisions: Vec::new(),
    }
}

async fn grouped_pm_status(
    service: &SuperagentService,
    store: &crate::Store,
    pm_threads: &[String],
    open_tickets: &[Ticket],
    ticket_statuses: &[TicketStatus],
) -> anyhow::Result<Vec<PmGroup>> {
    let mut groups = pm_threads
        .iter()
        .map(|pm_thread| pm_group(service, pm_thread))
        .collect::<Vec<_>>();
    let group_index: HashMap<&str, usize> = pm_threads
        .iter()
        .enumerate()
        .map(|(index, thread)| (thread.as_str(), index))
        .collect();
    let default_group = pm_threads
        .first()
        .and_then(|thread| group_index.get(thread.as_str()).copied());
    let mut ticket_groups = HashMap::new();
    let mut trigger_groups = HashMap::new();

    for (ticket, ticket_status) in open_tickets.iter().zip(ticket_statuses) {
        let pm = ticket_pm_thread(store, ticket, pm_threads, &mut trigger_groups).await?;
        let Some(index) = pm
            .as_deref()
            .and_then(|thread| group_index.get(thread).copied())
            .or(default_group)
        else {
            continue;
        };
        ticket_groups.insert(ticket.id.clone(), index);
        groups[index].ticket_count += 1;
        groups[index].ticket_names.push(
            ticket_status
                .name
                .clone()
                .unwrap_or_else(|| ticket.id.clone()),
        );
    }

    let decisions = store.recent_decisions(0, i64::MAX).await?;
    for decision in decisions {
        let pm = decision_pm_thread(
            store,
            &decision,
            &ticket_groups,
            pm_threads,
            &mut trigger_groups,
        )
        .await?;
        let Some(index) = pm
            .as_deref()
            .and_then(|thread| group_index.get(thread).copied())
            .or(default_group)
        else {
            continue;
        };
        if groups[index].recent_decisions.len() < 3 {
            groups[index].recent_decisions.push(decision);
        }
    }

    Ok(groups)
}

async fn ticket_pm_thread(
    store: &crate::Store,
    ticket: &Ticket,
    pm_threads: &[String],
    trigger_groups: &mut HashMap<String, Option<String>>,
) -> anyhow::Result<Option<String>> {
    if let Some(pm) = ticket.channels.iter().find_map(|channel| {
        pm_threads
            .iter()
            .find(|thread| thread.as_str() == channel.thread)
            .cloned()
    }) {
        return Ok(Some(pm));
    }
    let mut trigger_ids = Vec::new();
    if !ticket.origin.trigger_id.is_empty() {
        trigger_ids.push(&ticket.origin.trigger_id);
    }
    trigger_ids.extend(ticket.origin.trigger_ids.iter());
    for trigger_id in trigger_ids {
        if let Some(pm) = trigger_pm_thread(store, trigger_id, pm_threads, trigger_groups).await? {
            return Ok(Some(pm));
        }
    }
    Ok(None)
}

async fn decision_pm_thread(
    store: &crate::Store,
    decision: &Decision,
    ticket_groups: &HashMap<String, usize>,
    pm_threads: &[String],
    trigger_groups: &mut HashMap<String, Option<String>>,
) -> anyhow::Result<Option<String>> {
    for trigger_id in &decision.trigger_ids {
        if let Some(pm) = trigger_pm_thread(store, trigger_id, pm_threads, trigger_groups).await? {
            return Ok(Some(pm));
        }
    }
    if let Some(ticket_id) = &decision.ticket_id
        && let Some(index) = ticket_groups.get(ticket_id)
    {
        return Ok(pm_threads.get(*index).cloned());
    }
    if let Some(ticket_id) = &decision.ticket_id
        && let Some(ticket) = store.ticket(ticket_id).await?
    {
        return ticket_pm_thread(store, &ticket, pm_threads, trigger_groups).await;
    }
    Ok(None)
}

async fn trigger_pm_thread(
    store: &crate::Store,
    trigger_id: &str,
    pm_threads: &[String],
    trigger_groups: &mut HashMap<String, Option<String>>,
) -> anyhow::Result<Option<String>> {
    if let Some(pm) = trigger_groups.get(trigger_id) {
        return Ok(pm.clone());
    }
    let pm = store
        .trigger(trigger_id)
        .await?
        .as_ref()
        .and_then(|trigger| pm_thread_from_trigger(trigger, pm_threads));
    trigger_groups.insert(trigger_id.to_owned(), pm.clone());
    Ok(pm)
}

fn pm_thread_from_trigger(trigger: &Trigger, pm_threads: &[String]) -> Option<String> {
    trigger
        .channel
        .as_ref()
        .and_then(|channel| pm_threads.iter().find(|pm| pm.as_str() == channel.thread))
        .cloned()
        .or_else(|| {
            trigger
                .meta
                .get("thread")
                .and_then(serde_json::Value::as_str)
                .and_then(|thread| pm_threads.iter().find(|pm| pm.as_str() == thread))
                .cloned()
        })
}

/// One open ticket's line of the status.
pub fn ticket_status(ticket: &Ticket, now_ms: i64) -> TicketStatus {
    let latest = latest_assignment(ticket);
    let report = latest_report(ticket);
    let last_progress_at = [
        report.map(|r| r.at.clone()),
        latest.and_then(|a| a.delivered_at.clone()),
        latest.and_then(|a| a.idle_at.clone()),
    ]
    .into_iter()
    .flatten()
    .max()
    .or_else(|| (!ticket.created_at.is_empty()).then(|| ticket.created_at.clone()));
    let stage = ticket_stage(ticket).card();
    let waits = waits_for_person(ticket) || stage.starts_with("person");
    let stalled = !waits
        && last_progress_at
            .as_deref()
            .and_then(parse_ms)
            .is_some_and(|at| now_ms - at > STALL_MS);
    let last_activity = ticket.last_activity.clone();
    let last_activity_minutes = last_activity
        .as_ref()
        .and_then(|activity| activity.minutes_since(now_ms));
    TicketStatus {
        id: ticket.id.clone(),
        name: ticket.execution.as_ref().map(|e| e.name.clone()),
        title: ticket.title.clone(),
        stage,
        waits_for_person: waits,
        last_progress_at,
        stalled,
        last_activity,
        last_activity_minutes,
        report: report.map(|r| ReportStatus {
            state: r.state,
            summary: preview(&r.summary),
            at: r.at.clone(),
        }),
        thread_id: latest.and_then(|a| a.executor_ref.clone()),
        environment: latest.and_then(|a| a.environment.clone()),
        goal: None,
    }
}

/// The exit code of the design's table.
pub fn exit_code(status: &Status) -> i32 {
    if !status.started || status.pm_threads.is_empty() {
        return 3;
    }
    let part_in_error = status
        .health
        .values()
        .any(|health| health.state == HealthState::Error);
    if part_in_error || status.tickets.iter().any(|ticket| ticket.stalled) {
        return 2;
    }
    0
}

/// One line for the Mac launcher's banner; empty when everything is normal.
pub fn render_one_line(status: &Status) -> String {
    let mut problems = Vec::new();
    if !status.started {
        problems.push("引擎没有启动".to_owned());
    } else if status.pm_threads.is_empty() {
        problems.push("没有登记 PM 线程".to_owned());
    }
    for (name, health) in &status.health {
        if health.state == HealthState::Error {
            problems.push(format!("{name}：{}", health.detail));
        }
    }
    for ticket in &status.tickets {
        if ticket.stalled {
            let name = ticket.name.clone().unwrap_or_else(|| ticket.id.clone());
            problems.push(format!(
                "工单 {name} 自 {} 没有进展",
                ticket.last_progress_at.clone().unwrap_or_default()
            ));
        }
    }
    if problems.is_empty() {
        let waiting = status.attention.len();
        return if waiting > 0 {
            format!(
                "superagent 正常：{} 张在途工单，{waiting} 个问题等你回答",
                status.tickets.len()
            )
        } else {
            format!("superagent 正常：{} 张在途工单", status.tickets.len())
        };
    }
    format!(
        "superagent（退出码 {}）：{}",
        status.exit_code,
        problems.join("；")
    )
}

fn preview(text: &str) -> String {
    let mut out: String = text.chars().take(PREVIEW_CHARS).collect();
    if text.chars().count() > PREVIEW_CHARS {
        out.push('…');
    }
    out
}

fn parse_ms(at: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(at)
        .ok()
        .map(|t| t.timestamp_millis())
}

fn iso(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_default()
}
