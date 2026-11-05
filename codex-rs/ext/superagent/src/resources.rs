//! What `superagent_read` reads, a page at a time, for the PM and for a worker that may read only
//! its own ticket (prototype `src/app/resources.ts`).

use serde_json::Value;
use serde_json::json;

use crate::cards::ASSIGNMENTS;
use crate::cards::INPUTS;
use crate::cards::TicketThread;
use crate::cards::assignment_line;
use crate::cards::decision_card;
use crate::cards::decision_details;
use crate::cards::input_line;
use crate::cards::newest_page;
use crate::cards::preview;
use crate::cards::text_page;
use crate::cards::thread_card;
use crate::cards::ticket_card;
use crate::cards::ticket_details;
use crate::cards::trigger_card;
use crate::core::Ticket;
use crate::core::TicketInput;
use crate::core::Trigger;
use crate::core::UserError;
use crate::core::latest_report;
use crate::core::reached;
use crate::core::thread_key;
use crate::store::Store;

/// Default and most characters of a text page, and items of a list page.
pub const TEXT_DEFAULT: usize = 4000;
pub const TEXT_MAX: usize = 8000;
pub const LIST_DEFAULT: usize = 20;
pub const LIST_MAX: usize = 50;
const THREAD_DECISIONS: usize = 5;

/// Who reads: the PM, or a ticket's worker, which reads only its own ticket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadScope {
    Pm,
    Worker { ticket_id: String },
}

impl ReadScope {
    fn ticket_id(&self) -> Option<&str> {
        match self {
            Self::Pm => None,
            Self::Worker { ticket_id } => Some(ticket_id),
        }
    }
}

/// The resources, what each holds, and whether a worker may read it.
pub const RESOURCES: &[(&str, &str, bool)] = &[
    (
        "tickets",
        "ticket cards, newest first; a card's state is person/review after a ready report, person/blocked or person/failed after those reports, person/answer with a question, person/delivery when an input could not reach the worker, working or working/queued while it works, idle once the worker ended its turn without a report, else how the ticket ended",
        false,
    ),
    (
        "triggers",
        "trigger cards with where each went, newest first",
        false,
    ),
    (
        "threads",
        "subscribed threads, newest activity first, with their tickets and current assessment",
        false,
    ),
    (
        "thread/<key>",
        "one thread: its tickets and latest decisions, and its triggers, newest first",
        true,
    ),
    (
        "sessions",
        "every root session of this Codext, newest first: id, name, preview, cwd, run status and account; read without loading any; next_resource pages older ones",
        false,
    ),
    (
        "session/<id>",
        "one session's latest turns, newest first: what the person asked and what the assistant answered, shortened; read without resuming it",
        false,
    ),
    ("decisions", "decisions, newest first", false),
    ("decision/<id>", "one decision with its audit", false),
    (
        "ticket/<id>",
        "a ticket's card, the trigger, decision and threads it is tied to, its latest assignments with the start of each report, and its inputs, newest first",
        true,
    ),
    ("ticket/<id>/assignment", "its assignment text", true),
    (
        "ticket/<id>/assignments",
        "its assignments, newest first, with the start of each report",
        true,
    ),
    ("ticket/<id>/inputs", "its inputs, newest first", true),
    ("ticket/<id>/result", "its latest report", true),
    (
        "ticket/<id>/report/<id>",
        "the report of one of its assignments",
        true,
    ),
    ("ticket/<id>/input/<id>", "one input it was delivered", true),
    ("trigger/<id>", "a trigger's text and where it went", true),
];

/// The resource list a role's tool description offers.
pub fn resource_grammar(worker: bool) -> String {
    RESOURCES
        .iter()
        .filter(|(_, _, for_worker)| !worker || *for_worker)
        .map(|(name, holds, _)| format!("{name}: {holds}"))
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    User(#[from] UserError),
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

fn refused(text: &str) -> ReadError {
    ReadError::Refused(text.to_owned())
}

struct Read<'a> {
    store: &'a Store,
    scope: &'a ReadScope,
    resource: String,
    offset: usize,
    limit: Option<usize>,
}

impl Read<'_> {
    fn pm(&self) -> Result<(), ReadError> {
        match self.scope {
            ReadScope::Pm => Ok(()),
            ReadScope::Worker { .. } => Err(refused("This resource belongs to the PM")),
        }
    }

    fn text(&self, body: &str) -> Value {
        let limit = self.limit.unwrap_or(TEXT_DEFAULT).clamp(1, TEXT_MAX);
        text_page(&self.resource, body, self.offset, limit)
    }

    fn list(&self, items: Vec<Value>) -> Value {
        let size = self.limit.unwrap_or(LIST_DEFAULT).clamp(1, LIST_MAX);
        let total = items.len();
        let page: Vec<Value> = items.into_iter().skip(self.offset).take(size).collect();
        json!({
            "items": page,
            "next_offset": if self.offset + size < total { json!(self.offset + size) } else { Value::Null },
        })
    }
}

/// Whether a worker's ticket may read a Trigger: it reached the ticket, was in its origin batch,
/// or is on a Thread the ticket follows.
async fn readable(store: &Store, ticket: &Ticket, trigger: &Trigger) -> anyhow::Result<bool> {
    let mut threads = Vec::new();
    for reference in &trigger.threads {
        threads.push(store.thread(&thread_key(reference)).await?);
    }
    let lookup = |key: &str| {
        trigger
            .threads
            .iter()
            .zip(threads.iter())
            .find(|(reference, _)| thread_key(reference) == key)
            .and_then(|(_, thread)| thread.clone())
    };
    Ok(reached(ticket, trigger, lookup, true)
        || ticket.origin.trigger_ids.contains(&trigger.id)
        || trigger.threads.iter().any(|reference| {
            ticket
                .follows
                .as_ref()
                .is_some_and(|follows| follows.iter().any(|f| f.thread == thread_key(reference)))
        }))
}

async fn trigger_for(read: &Read<'_>, id: &str) -> Result<Trigger, ReadError> {
    let Some(found) = read.store.trigger(id).await? else {
        return Err(UserError::new("unknownTrigger", &[]).into());
    };
    if let Some(ticket_id) = read.scope.ticket_id() {
        let ticket = read.store.ticket(ticket_id).await?;
        let allowed = match ticket {
            Some(ticket) => readable(read.store, &ticket, &found).await?,
            None => false,
        };
        if !allowed {
            return Err(UserError::new("notReached", &[]).into());
        }
    }
    Ok(found)
}

async fn thread_label(store: &Store, key: &str) -> anyhow::Result<String> {
    Ok(match store.first_trigger(key, true).await? {
        Some(trigger) => preview(&trigger.text),
        None => key.to_owned(),
    })
}

fn trigger_line(trigger: &Trigger) -> Value {
    json!({
        "trigger_id": trigger.id,
        "actor": trigger.actor.id,
        "human": trigger.human,
        "at": trigger.at,
        "preview": preview(&trigger.text),
        "chars": trigger.text.chars().count(),
        "read": format!("trigger/{}", trigger.id),
    })
}

/// How a worker reads an Input: a Trigger's text with its note, or the text itself.
async fn input_text(store: &Store, input: &TicketInput) -> anyhow::Result<String> {
    Ok(match input {
        TicketInput::Trigger {
            trigger_id, note, ..
        } => {
            let text = store
                .trigger(trigger_id)
                .await?
                .map(|t| t.text)
                .unwrap_or_default();
            match note {
                Some(note) => format!("{text}\n\n[note] {note}"),
                None => text,
            }
        }
        TicketInput::Resumed { trigger_id, .. } => {
            format!(
                "[resumed by] {}",
                store
                    .trigger(trigger_id)
                    .await?
                    .map(|t| t.text)
                    .unwrap_or_default()
            )
        }
        TicketInput::Text { text, .. } | TicketInput::Note { text, .. } => text.clone(),
        TicketInput::PolicyChanged { from, to, .. } => format!("[policy changed] {from} -> {to}"),
        TicketInput::Reopen { decision_id, .. } => format!("[reopened by decision] {decision_id}"),
    })
}

/// Reads one page of a resource for the PM or a ticket's worker.
pub async fn read_resource(
    store: &Store,
    scope: &ReadScope,
    resource: &str,
    offset: usize,
    limit: Option<usize>,
) -> Result<Value, ReadError> {
    let read = Read {
        store,
        scope,
        resource: resource.to_owned(),
        offset,
        limit,
    };
    if resource == "tickets" {
        read.pm()?;
        let cards = store.tickets().await?.iter().map(ticket_card).collect();
        return Ok(read.list(cards));
    }
    if resource == "triggers" {
        read.pm()?;
        let mut cards = Vec::new();
        for trigger in store.recent_triggers(0, (LIST_MAX + offset) as i64).await? {
            let disposition = store.disposition(&trigger.id).await?;
            cards.push(trigger_card(&trigger, disposition.as_ref()));
        }
        return Ok(read.list(cards));
    }
    if resource == "threads" {
        read.pm()?;
        let mut threads = store.subscribed_threads(None).await?;
        threads.sort_by(|a, b| {
            let a_at = a.last_at.clone().unwrap_or_else(|| a.created_at.clone());
            let b_at = b.last_at.clone().unwrap_or_else(|| b.created_at.clone());
            b_at.cmp(&a_at)
        });
        let mut cards = Vec::new();
        for thread in &threads {
            cards.push(thread_card(
                thread,
                &thread_label(store, &thread.key).await?,
            ));
        }
        return Ok(read.list(cards));
    }
    if resource == "decisions" {
        read.pm()?;
        let cards = store
            .recent_decisions(0, (LIST_MAX + offset) as i64)
            .await?
            .iter()
            .map(decision_card)
            .collect();
        return Ok(read.list(cards));
    }
    if let Some(id) = resource.strip_prefix("decision/") {
        read.pm()?;
        let Some(decision) = store.decision(id).await? else {
            return Err(refused("Unknown decision"));
        };
        return Ok(decision_details(&decision));
    }
    if let Some(key) = resource.strip_prefix("thread/") {
        let Some(thread) = store.thread(key).await? else {
            return Err(refused("Unknown or inaccessible thread"));
        };
        if let Some(ticket_id) = scope.ticket_id() {
            let ticket = store.ticket(ticket_id).await?;
            let follows = ticket
                .as_ref()
                .and_then(|t| t.follows.as_ref())
                .is_some_and(|follows| follows.iter().any(|f| f.thread == key));
            if !(thread.subscriptions.iter().any(|id| id == ticket_id) || follows) {
                return Err(refused("Unknown or inaccessible thread"));
            }
        }
        let mut card = thread_card(&thread, &thread_label(store, key).await?);
        if let Some(map) = card.as_object_mut() {
            map.remove("read");
            map.remove("tickets");
        }
        let lines: Vec<Value> = store
            .thread_page(key, 0, (LIST_MAX + offset) as i64)
            .await?
            .iter()
            .map(trigger_line)
            .collect();
        let page = read.list(lines);
        if scope.ticket_id().is_some() {
            merge(&mut card, page);
            return Ok(card);
        }
        let mut tickets = Vec::new();
        for id in &thread.subscriptions {
            if let Some(ticket) = store.ticket(id).await? {
                tickets.push(ticket_card(&ticket));
            }
        }
        let decisions: Vec<Value> = store
            .decisions_by_thread(key)
            .await?
            .iter()
            .rev()
            .take(THREAD_DECISIONS)
            .map(decision_card)
            .collect();
        card["tickets"] = json!(tickets);
        card["decisions"] = json!(decisions);
        merge(&mut card, page);
        return Ok(card);
    }
    if let Some(id) = resource.strip_prefix("trigger/") {
        if id.is_empty() || id.contains('/') {
            return Err(refused("Unknown context resource"));
        }
        let found = trigger_for(&read, id).await?;
        let disposition = store.disposition(&found.id).await?;
        // A worker sees neither decision ids nor the other tickets a message reached.
        let visible = match (disposition, scope.ticket_id()) {
            (Some(mut d), Some(ticket_id)) => {
                d.decision_id = None;
                d.fanout.retain(|t| t == ticket_id);
                Some(d)
            }
            (d, _) => d,
        };
        let mut card = trigger_card(&found, visible.as_ref());
        if let Some(map) = card.as_object_mut() {
            map.remove("preview");
            map.remove("chars");
            map.remove("read");
        }
        merge(&mut card, read.text(&found.text));
        return Ok(card);
    }
    if let Some(rest) = resource.strip_prefix("ticket/") {
        let parts: Vec<&str> = rest.split('/').collect();
        let (id, section, part) = match parts.as_slice() {
            [id] => (*id, None, None),
            [id, section] => (*id, Some(*section), None),
            [id, section, part] => (*id, Some(*section), Some(*part)),
            _ => return Err(refused("Unknown context resource")),
        };
        if id.is_empty() {
            return Err(refused("Unknown context resource"));
        }
        if let Some(ticket_id) = scope.ticket_id()
            && ticket_id != id
        {
            return Err(refused("Ticket does not belong to this worker"));
        }
        let Some(ticket) = store.ticket(id).await? else {
            return Err(UserError::new("unknownTicket", &[]).into());
        };
        let worker = scope.ticket_id().is_some();
        match (section, part) {
            (None, _) => {
                let bound: Vec<String> = ticket
                    .follows
                    .iter()
                    .flatten()
                    .map(|f| f.thread.clone())
                    .collect();
                let subscribed: Vec<_> = store
                    .subscribed_threads(None)
                    .await?
                    .into_iter()
                    .filter(|thread| thread.subscriptions.iter().any(|t| t == id))
                    .collect();
                let mut threads: Vec<TicketThread> = subscribed
                    .iter()
                    .map(|thread| TicketThread {
                        thread: thread.key.clone(),
                        context: thread
                            .context
                            .as_ref()
                            .is_some_and(|c| c.iter().any(|t| t == id)),
                        bound: bound.contains(&thread.key),
                    })
                    .collect();
                for key in &bound {
                    if !subscribed.iter().any(|t| &t.key == key) {
                        threads.push(TicketThread {
                            thread: key.clone(),
                            context: false,
                            bound: true,
                        });
                    }
                }
                let mut batch = Vec::new();
                for trigger_id in &ticket.origin.trigger_ids {
                    if let Some(trigger) = store.trigger(trigger_id).await? {
                        batch.push(trigger);
                    }
                }
                Ok(ticket_details(&ticket, &threads, &batch, worker))
            }
            (Some("assignments"), None) => {
                let size = read.limit.unwrap_or(ASSIGNMENTS).clamp(1, LIST_MAX);
                let mut page = newest_page(
                    &ticket.assignments,
                    offset,
                    size,
                    |a| assignment_line(&ticket, a),
                    resource,
                );
                page["next_offset"] = page.get("next_offset").cloned().unwrap_or(Value::Null);
                Ok(page)
            }
            (Some("inputs"), None) => {
                let size = read.limit.unwrap_or(INPUTS).clamp(1, LIST_MAX);
                let mut page = newest_page(
                    &ticket.inputs,
                    offset,
                    size,
                    |i| input_line(&ticket, i, worker),
                    resource,
                );
                page["next_offset"] = page.get("next_offset").cloned().unwrap_or(Value::Null);
                Ok(page)
            }
            (Some("input"), Some(input_id)) => {
                let Some(input) = ticket.inputs.iter().find(|i| i.id() == input_id) else {
                    return Err(refused("Unknown ticket input"));
                };
                let body = input_text(store, input).await?;
                Ok(read.text(&body))
            }
            (Some("report"), Some(assignment_id)) => {
                let Some(report) = ticket
                    .assignments
                    .iter()
                    .find(|a| a.id == assignment_id)
                    .and_then(|a| a.report.as_ref())
                else {
                    return Err(refused("Unknown assignment report"));
                };
                Ok(read.text(&report.summary))
            }
            (Some("assignment"), None) => Ok(read.text(
                ticket
                    .execution
                    .as_ref()
                    .map_or("", |e| e.assignment.as_str()),
            )),
            (Some("result"), None) => {
                Ok(read.text(latest_report(&ticket).map_or("", |r| r.summary.as_str())))
            }
            _ => Err(refused("Unknown context resource")),
        }
    } else {
        Err(refused("Unknown context resource"))
    }
}

fn merge(into: &mut Value, from: Value) {
    if let (Some(target), Value::Object(source)) = (into.as_object_mut(), from) {
        for (k, v) in source {
            target.insert(k, v);
        }
    }
}
