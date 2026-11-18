//! Compact, bounded projections of triggers, tickets, threads and decisions for models; full texts
//! stay behind `superagent_read` (prototype `src/app/cards.ts`, the card parts of `notices.ts` and
//! `links.ts`).

use chrono::Utc;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

use crate::core::Assignment;
use crate::core::Decision;
use crate::core::Mandate;
use crate::core::Strength;
use crate::core::Thread;
use crate::core::Ticket;
use crate::core::TicketInput;
use crate::core::Trigger;
use crate::core::TriggerDisposition;
use crate::core::latest_report;
use crate::core::reason_text;
use crate::core::stage::ticket_stage;
use crate::core::thread_key;
use crate::ids::key;

/// The preview length of a message, in characters.
pub const PREVIEW_CHARS: usize = 160;
/// The most characters of a report summary a relay carries inline.
pub const SUMMARY_CHARS: usize = 600;
/// How many Inputs and Assignments a ticket's page shows first.
pub const INPUTS: usize = 10;
pub const ASSIGNMENTS: usize = 3;
const INPUT_TEXT: usize = 80;
/// The most related tickets a candidate card lists.
pub const RELATED_TICKETS: usize = 8;

/// The states a ticket card shows: an open ticket's stage, and an ended ticket's outcome.
pub const TICKET_STATES: &[&str] = &[
    "person/delivery",
    "person/answer",
    "person/review",
    "person/blocked",
    "person/failed",
    "working",
    "working/queued",
    "idle",
    "completed",
    "merged",
    "closed",
    "dropped",
    "stopped",
];

/// The first `limit` characters of `text`, with whitespace collapsed.
pub fn truncate_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Collapses whitespace and keeps the first 160 characters.
pub fn preview(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&collapsed, PREVIEW_CHARS)
}

pub fn trigger_card(trigger: &Trigger, disposition: Option<&TriggerDisposition>) -> Value {
    let mut card = json!({
        "trigger_id": trigger.id,
        "source": trigger.source,
        "actor": trigger.actor.id,
        "thread": trigger.threads.first().map(thread_key).unwrap_or_default(),
        "human": trigger.human,
        "at": trigger.at,
        "preview": preview(&trigger.text),
        "chars": trigger.text.chars().count(),
        "read": format!("trigger/{}", trigger.id),
    });
    if let Some(url) = &trigger.url {
        card["source_url"] = json!(url);
    }
    if let Some(disposition) = disposition {
        let mut went = Map::new();
        if let Some(filtered) = &disposition.filtered {
            went.insert(
                "filtered".into(),
                json!(reason_text(std::slice::from_ref(filtered))),
            );
        }
        if let Some(decision_id) = &disposition.decision_id {
            went.insert("decision_id".into(), json!(decision_id));
        }
        went.insert("delivered_to".into(), json!(disposition.fanout));
        card["disposition"] = Value::Object(went);
    }
    card
}

/// A batch member as the PM sees it: who said what, in 160 characters, and where to read the rest.
pub fn batch_line(trigger: &Trigger) -> Value {
    json!({
        "trigger_id": trigger.id,
        "from": trigger.actor.id,
        "at": trigger.at,
        "preview": preview(&trigger.text),
        "read": format!("trigger/{}", trigger.id),
    })
}

/// The closed card state of a ticket.
pub fn ticket_state(ticket: &Ticket) -> String {
    match &ticket.ended {
        Some(ended) => serde_json::to_value(ended.outcome)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
        None => ticket_stage(ticket).card(),
    }
}

fn strength_name(strength: Strength) -> &'static str {
    match strength {
        Strength::Explicit => "explicit",
        Strength::Inferred => "inferred",
    }
}

pub fn ticket_card(ticket: &Ticket) -> Value {
    let mut card = json!({
        "ticket_id": ticket.id,
        "title": ticket.title,
        "state": ticket_state(ticket),
        "strength": strength_name(ticket.mandate.strength),
        "preview": preview(latest_report(ticket).map_or("", |r| r.summary.as_str())),
        "read": format!("ticket/{}", ticket.id),
        "lastActivity": ticket.last_activity,
        "lastActivityMinutes": ticket
            .last_activity
            .as_ref()
            .and_then(|activity| activity.minutes_since(Utc::now().timestamp_millis())),
    });
    if let Some(ended) = &ticket.ended {
        card["ended"] = json!({
            "at": ended.at,
            "by": ended.by,
            "reason": ended.reason.clone().unwrap_or_else(|| serde_json::to_value(ended.outcome)
                .ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()),
        });
    }
    if let Some(attention) = &ticket.attention {
        let mut asked = json!({ "question": attention.question });
        if let Some(trigger_id) = &attention.trigger_id {
            asked["trigger_id"] = json!(trigger_id);
        }
        card["attention"] = asked;
    }
    card
}

/// A ticket card without the report preview, for relays that carry the summary themselves.
pub fn brief_ticket(ticket: &Ticket) -> Value {
    let mut card = ticket_card(ticket);
    if let Some(map) = card.as_object_mut() {
        map.remove("preview");
    }
    card
}

/// Strips `policyVersion`, an internal hash nobody queries by.
fn mandate_of(mandate: &Mandate) -> Value {
    let mut value = serde_json::to_value(mandate).unwrap_or(Value::Null);
    if let Some(map) = value.as_object_mut() {
        map.remove("policyVersion");
    }
    value
}

pub fn input_resource(ticket_id: &str, input_id: &str) -> String {
    format!("ticket/{ticket_id}/input/{input_id}")
}

/// One Input on a ticket's page: inline when short, else a reference; decision ids are kept from workers.
pub fn input_line(ticket: &Ticket, input: &TicketInput, worker: bool) -> Value {
    let kind = match input {
        TicketInput::Trigger { .. } => "trigger",
        TicketInput::PolicyChanged { .. } => "policy-changed",
        TicketInput::Reopen { .. } => "reopen",
        TicketInput::Resumed { .. } => "resumed",
        TicketInput::Text { .. } => "text",
        TicketInput::Note { .. } => "note",
    };
    let mut line = json!({ "kind": kind, "at": input.at() });
    match input {
        TicketInput::Trigger { trigger_id, .. } | TicketInput::Resumed { trigger_id, .. } => {
            line["trigger_id"] = json!(trigger_id);
        }
        TicketInput::Reopen { decision_id, .. } if !worker => {
            line["decision_id"] = json!(decision_id);
        }
        TicketInput::Text { text, .. } | TicketInput::Note { text, .. } => {
            if text.chars().count() <= INPUT_TEXT {
                line["text"] = json!(text);
            } else {
                line["preview"] = json!(truncate_chars(&preview(text), INPUT_TEXT));
                line["chars"] = json!(text.chars().count());
                line["read"] = json!(input_resource(&ticket.id, input.id()));
            }
        }
        _ => {
            line["read"] = json!(input_resource(&ticket.id, input.id()));
        }
    }
    if ticket.undelivered.iter().any(|id| id == input.id()) {
        line["undelivered"] = json!(true);
    }
    line
}

pub fn assignment_line(ticket: &Ticket, assignment: &Assignment) -> Value {
    let mut line =
        json!({ "id": assignment.id, "state": assignment.state, "at": assignment.created_at });
    if let Some(reason) = &assignment.ended_reason {
        line["ended_reason"] = json!(reason_text(&[crate::core::Reason::code(reason)]));
    }
    if let Some(report) = &assignment.report {
        line["report"] = json!({
            "state": report.state,
            "at": report.at,
            "preview": truncate_chars(&preview(&report.summary), 100),
            "chars": report.summary.chars().count(),
            "read": format!("ticket/{}/report/{}", ticket.id, assignment.id),
        });
    }
    line
}

/// A page of the newest items first: `items`, and where the next page is when there is one.
pub fn newest_page<T>(
    items: &[T],
    offset: usize,
    size: usize,
    line: impl Fn(&T) -> Value,
    next: &str,
) -> Value {
    let newest: Vec<&T> = items.iter().rev().collect();
    let page: Vec<Value> = newest
        .iter()
        .skip(offset)
        .take(size)
        .map(|item| line(item))
        .collect();
    let mut value = json!({ "items": page });
    if offset + size < newest.len() {
        value["next_resource"] = json!(next);
        value["next_offset"] = json!(offset + size);
    }
    value
}

/// A Thread a ticket is tied to: followed only for context, or bound by a Source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TicketThread {
    pub thread: String,
    pub context: bool,
    pub bound: bool,
}

/// A ticket with the ids it is tied to and the first pages of its assignments and inputs.
pub fn ticket_details(
    ticket: &Ticket,
    threads: &[TicketThread],
    batch: &[Trigger],
    worker: bool,
) -> Value {
    let mut map = match brief_ticket(ticket) {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    map.insert("trigger_id".into(), json!(ticket.origin.trigger_id));
    if !batch.is_empty() {
        map.insert(
            "batch".into(),
            json!(batch.iter().map(batch_line).collect::<Vec<_>>()),
        );
    }
    if !worker {
        map.insert("decision_id".into(), json!(ticket.decision_id));
    }
    map.insert(
        "threads".into(),
        json!(
            threads
                .iter()
                .map(|t| {
                    let mut value = json!({ "thread": t.thread });
                    if t.context {
                        value["context"] = json!(true);
                    }
                    if t.bound {
                        value["bound"] = json!(true);
                    }
                    value
                })
                .collect::<Vec<_>>()
        ),
    );
    map.insert(
        "directory".into(),
        json!(ticket.execution.as_ref().map(|e| e.directory.clone())),
    );
    map.insert("scope".into(), json!(ticket.mandate.scope));
    map.insert("requested_by".into(), json!(ticket.mandate.actor.id));
    map.insert(
        "assignments".into(),
        newest_page(
            &ticket.assignments,
            0,
            ASSIGNMENTS,
            |a| assignment_line(ticket, a),
            &format!("ticket/{}/assignments", ticket.id),
        ),
    );
    map.insert(
        "inputs".into(),
        newest_page(
            &ticket.inputs,
            0,
            INPUTS,
            |i| input_line(ticket, i, worker),
            &format!("ticket/{}/inputs", ticket.id),
        ),
    );
    map.insert(
        "assignment".into(),
        json!(format!("ticket/{}/assignment", ticket.id)),
    );
    map.insert(
        "result".into(),
        json!(format!("ticket/{}/result", ticket.id)),
    );
    Value::Object(map)
}

pub fn thread_card(thread: &Thread, label: &str) -> Value {
    let mut card = json!({
        "thread": thread.key,
        "label": label,
        "source": thread.reference.source,
        "tickets": thread.subscriptions,
        "pending": thread.pending.len(),
        "read": format!("thread/{}", thread.key),
    });
    if let Some(evaluating) = &thread.evaluating {
        card["evaluating"] =
            json!({ "since": evaluating.started_at, "deadline": evaluating.deadline });
    }
    if let Some(url) = thread.meta.get("url").and_then(Value::as_str) {
        card["url"] = json!(url);
    }
    card
}

pub fn decision_card(decision: &Decision) -> Value {
    let mut card = json!({
        "decision_id": decision.id,
        "kind": decision.kind,
        "reason": reason_text(&decision.reason),
        "strength": strength_name(decision.strength),
        "thread": decision.thread_key,
        "trigger_ids": decision.trigger_ids,
        "at": decision.at,
        "read": format!("decision/{}", decision.id),
    });
    if let Some(ticket_id) = &decision.ticket_id {
        card["ticket_id"] = json!(ticket_id);
    }
    card
}

pub fn decision_details(decision: &Decision) -> Value {
    let mut details = decision_card(decision);
    if let Some(mandate) = &decision.mandate {
        details["mandate"] = mandate_of(mandate);
    }
    if let Some(audit) = &decision.audit {
        details["audit"] = Value::Object(audit.clone());
    }
    details
}

/// A page of a text by characters, so the pages joined give the text back.
pub fn text_page(resource: &str, text: &str, offset: usize, limit: usize) -> Value {
    let total = text.chars().count();
    let end = total.min(offset.saturating_add(limit));
    let page: String = text
        .chars()
        .skip(offset)
        .take(end.saturating_sub(offset))
        .collect();
    json!({
        "resource": resource,
        "offset": offset,
        "total_chars": total,
        "next_offset": if end < total { json!(end) } else { Value::Null },
        "text": page,
    })
}

/// The tickets of a candidate's Thread, open ones first and newest first, at most eight.
pub fn related_tickets(tickets: &[Ticket]) -> (Vec<Value>, usize) {
    let mut order: Vec<&Ticket> = tickets.iter().collect();
    order.sort_by(|a, b| {
        let open = a.ended.is_none().cmp(&b.ended.is_none()).reverse();
        open.then_with(|| b.created_at.cmp(&a.created_at))
    });
    (
        order
            .into_iter()
            .take(RELATED_TICKETS)
            .map(ticket_card)
            .collect(),
        tickets.len(),
    )
}

/// The next ticket number: tNNN counts up from the tickets that exist.
pub fn next_ticket_number(existing: usize) -> String {
    format!("t{:03}", existing + 1)
}

/// The text a summary over 600 characters becomes: the first 600, its length, and where to read the rest.
fn bounded_summary(text: &str, read: &str) -> Map<String, Value> {
    let mut map = Map::new();
    let chars = text.chars().count();
    if chars > SUMMARY_CHARS {
        map.insert("summary".into(), json!(truncate_chars(text, SUMMARY_CHARS)));
        map.insert("chars".into(), json!(chars));
    } else {
        map.insert("summary".into(), json!(text));
    }
    map.insert("read".into(), json!(read));
    map
}

/// A relay with the request id that makes its delivery happen once, and its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relay {
    pub id: String,
    pub text: String,
}

/// A worker's report as the PM hears it: the ticket card, the state, up to 600 characters of the
/// summary and where to read the rest; `answer_to` names the person's question it answers.
/// The four lists of a worker's five-line report, beside the summary.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReportDetails {
    #[serde(default)]
    pub changes: Vec<String>,
    #[serde(default)]
    pub tests_run: Vec<String>,
    #[serde(default)]
    pub risks: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
}

impl ReportDetails {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
            && self.tests_run.is_empty()
            && self.risks.is_empty()
            && self.evidence.is_empty()
    }
}

pub fn worker_report_relay(
    ticket: &Ticket,
    state: &str,
    summary: &str,
    answer_to: Option<&str>,
    details: Option<&ReportDetails>,
) -> Relay {
    let id = key(["worker-result", &ticket.id, &ticket.updated_at, summary]);
    let mut report = Map::new();
    report.insert("kind".into(), json!("worker_report"));
    report.insert("ticket".into(), brief_ticket(ticket));
    report.insert("trigger_id".into(), json!(ticket.origin.trigger_id));
    report.insert("state".into(), json!(state));
    if let Some(answer_to) = answer_to {
        report.insert("answer_to".into(), json!(answer_to));
    }
    if let Some(details) = details {
        for (name, items) in [
            ("changes", &details.changes),
            ("tests_run", &details.tests_run),
            ("risks", &details.risks),
            ("evidence", &details.evidence),
        ] {
            if !items.is_empty() {
                report.insert(name.into(), json!(items));
            }
        }
    }
    report.extend(bounded_summary(
        summary,
        &format!("ticket/{}/result", ticket.id),
    ));
    Relay {
        id: format!("report:{id}"),
        text: Value::Object(report).to_string(),
    }
}

/// A question for a person, as the PM hears it: the card alone, the question being on it.
pub fn worker_attention_relay(ticket: &Ticket, attention_id: &str, state: &str) -> Relay {
    Relay {
        id: format!("attention:{}:{attention_id}", ticket.id),
        text: json!({
            "kind": "worker_attention",
            "ticket": brief_ticket(ticket),
            "state": state,
            "read": format!("ticket/{}/result", ticket.id),
        })
        .to_string(),
    }
}

/// What a follow-up asks of the PM: nothing but a stop the person asks for; its ticket's worker decides it.
pub const FOLLOWUP_NOTE: &str = "Its ticket's worker has it and decides; act only if the person asks to stop the work (superagent_route kind stop with this trigger_id).";

/// A person's message that reached tickets, as the PM hears of it.
pub fn follow_up_relay(trigger: &Trigger, tickets: &[(String, String, String, bool)]) -> Relay {
    let delivered: Vec<Value> = tickets
        .iter()
        .map(|(ticket_id, title, state, context)| {
            let mut value = json!({ "ticket_id": ticket_id, "title": title, "state": state });
            if *context {
                value["context"] = json!(true);
            }
            value
        })
        .collect();
    let mut message = trigger_card(trigger, None);
    if let Some(map) = message.as_object_mut() {
        map.insert("kind".into(), json!("follow_up"));
        map.insert("delivered_to".into(), json!(delivered));
        map.insert("note".into(), json!(FOLLOWUP_NOTE));
    }
    Relay {
        id: format!("followup:{}", trigger.id),
        text: message.to_string(),
    }
}

/// A notice the PM reads at its next turn: a coalescing name, the value and a one-line summary.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub name: String,
    pub value: Value,
    pub summary: String,
}

pub fn ticket_ended_notice(ticket: &Ticket) -> Notice {
    let reason = ticket
        .ended
        .as_ref()
        .and_then(|e| e.reason.clone())
        .unwrap_or_else(|| ticket_state(ticket));
    Notice {
        name: key(["receipt", "ended", &ticket.id, &ticket.updated_at]),
        value: json!({ "ticket_ended": brief_ticket(ticket) }),
        summary: format!("Ticket {} ended ({reason})", ticket.id),
    }
}

pub fn trigger_requeued_notice(trigger_id: &str, at: &str) -> Notice {
    Notice {
        name: key(["receipt", "requeued", trigger_id, at]),
        value: json!({ "trigger_requeued": trigger_id, "read": format!("trigger/{trigger_id}"), "at": at }),
        summary: format!("Trigger {trigger_id} queued for a fresh assessment"),
    }
}

/// What a candidate card is built from, beyond the candidate itself.
pub struct CandidateContext<'a> {
    /// Each batch Trigger's disposition, for the tickets it reached and the hand-backs.
    pub dispositions: Vec<(&'a Trigger, Option<&'a TriggerDisposition>)>,
    /// The tickets the primary Trigger's text names, at most four.
    pub referenced: Vec<&'a Ticket>,
    /// The number of tickets that exist, for the next ticket number.
    pub tickets_total: usize,
    /// The accept mode of the Policy.
    pub mode: &'a str,
    /// The tickets that handed a batch Trigger back: their name, directory, scope and assignment go
    /// on the card, so the PM can pass the context on to a new ticket.
    pub origins: Vec<&'a Ticket>,
    /// The origin tickets' original requests (trigger text), by ticket id, at most 300 characters.
    pub origin_texts: std::collections::HashMap<String, String>,
}

/// The candidate card the PM decides on (design §5.1): a header line it can see at a glance, then
/// the card as JSON with the original text bounded to 600 characters.
pub fn candidate_card(
    eval_id: &str,
    triggers: &[Trigger],
    strength: Strength,
    tickets: &[Ticket],
    context: &CandidateContext<'_>,
) -> String {
    let Some(primary) = triggers.last() else {
        return format!("[候选卡 eval={eval_id}] （空）");
    };
    let delivered: Vec<String> = context
        .dispositions
        .iter()
        .find(|(t, _)| t.id == primary.id)
        .and_then(|(_, d)| d.map(|d| d.fanout.clone()))
        .unwrap_or_default();
    let handed_back: Vec<Value> = context
        .dispositions
        .iter()
        .flat_map(|(trigger, disposition)| {
            disposition
                .and_then(|d| d.handbacks.as_ref())
                .map(|items| {
                    items
                        .iter()
                        .map(|h| {
                            let mut item = Map::new();
                            item.insert("trigger_id".into(), json!(trigger.id));
                            item.insert("ticket_id".into(), json!(h.ticket_id));
                            item.insert("reason".into(), json!(h.reason));
                            if let Some(text) = context.origin_texts.get(&h.ticket_id) {
                                item.insert("origin_text".into(), json!(text));
                            }
                            if let Some(origin) =
                                context.origins.iter().find(|t| t.id == h.ticket_id)
                            {
                                item.insert("ticket_scope".into(), json!(origin.mandate.scope));
                                if let Some(execution) = &origin.execution {
                                    item.insert("ticket_name".into(), json!(execution.name));
                                    item.insert(
                                        "ticket_directory".into(),
                                        json!(execution.directory),
                                    );
                                    item.insert(
                                        "assignment".into(),
                                        json!(
                                            execution
                                                .assignment
                                                .chars()
                                                .take(300)
                                                .collect::<String>()
                                        ),
                                    );
                                }
                            }
                            Value::Object(item)
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect();
    let others: Vec<Value> = triggers
        .iter()
        .filter(|t| t.id != primary.id)
        .map(|t| trigger_card(t, None))
        .collect();
    let (related, total) = related_tickets(tickets);
    let strength_text = match strength {
        Strength::Explicit => "明确",
        Strength::Inferred => "推断",
    };
    let mut card = Map::new();
    card.insert("eval_id".into(), json!(eval_id));
    card.insert("trigger_id".into(), json!(primary.id));
    card.insert("from".into(), json!(primary.actor.id));
    card.insert("at".into(), json!(primary.at));
    card.insert(
        "thread".into(),
        json!(primary.threads.first().map(thread_key).unwrap_or_default()),
    );
    card.extend(bounded_summary(
        &primary.text,
        &format!("trigger/{}", primary.id),
    ));
    if let Some(text) = card.remove("summary") {
        card.insert("text".into(), text);
    }
    card.insert("strength".into(), json!(strength_name(strength)));
    card.insert("mode".into(), json!(context.mode));
    if !others.is_empty() {
        card.insert("batch".into(), json!(others));
    }
    if !handed_back.is_empty() {
        card.insert("handed_back".into(), json!(handed_back));
    }
    card.insert("related_tickets".into(), json!(related));
    card.insert("tickets_total".into(), json!(total));
    if !delivered.is_empty() {
        card.insert("delivered_to".into(), json!(delivered));
    }
    if !context.referenced.is_empty() {
        card.insert(
            "referenced_tickets".into(),
            json!(
                context
                    .referenced
                    .iter()
                    .take(4)
                    .map(|t| ticket_card(t))
                    .collect::<Vec<_>>()
            ),
        );
    }
    card.insert(
        "next_ticket_number".into(),
        json!(next_ticket_number(context.tickets_total)),
    );
    card.insert(
        "next_action".into(),
        json!(match strength {
            Strength::Explicit => "用 superagent_route 处理：accept、attach、stop 或 ignore。",
            Strength::Inferred => "推断的候选：没有规则命中就 ignore；A2 之前不接受。",
        }),
    );
    format!(
        "[候选卡 eval={eval_id}] 触发 {} · 强度 {strength_text} · 模式 {}\n{}",
        primary.id,
        context.mode,
        Value::Object(card)
    )
}
