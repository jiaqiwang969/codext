//! Thread rules R1–R7 (design §3): which Thread a message spoken in codext belongs to.
//!
//! The PM's one long conversation holds many tickets, so a message is not a Thread of its own
//! conversation: a message that names a ticket (R1), or is spoken in a worker's conversation (R2),
//! belongs to that ticket's Thread; any other message opens a Thread of its own (R3). Further
//! ticket names in the text are mentions, kept as context only (R4). Build events belong to their
//! branch's Thread (R6). Background-command ends are Inputs, not Triggers (R7).

use super::model::ThreadRef;

pub const SOURCE: &str = "codext";

/// Where a message was spoken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Spoken {
    /// In a PM's conversation.
    Pm { pm_thread: String },
    /// In the root thread of a ticket's worker.
    Worker { ticket_id: String },
}

/// Where a message belongs: its Thread, and the other tickets it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub thread: ThreadRef,
    /// Ticket ids named in the text beyond the one the message belongs to, in order of appearance.
    pub mentions: Vec<String>,
}

/// The Thread of a ticket.
pub fn ticket_thread(ticket_id: &str) -> ThreadRef {
    ThreadRef {
        source: SOURCE.to_owned(),
        id: format!("ticket/{ticket_id}"),
    }
}

/// The Thread a new request opens (R3).
pub fn request_thread(trigger_id: &str) -> ThreadRef {
    ThreadRef {
        source: SOURCE.to_owned(),
        id: format!("pm/{trigger_id}"),
    }
}

/// The Thread of a candidate build (R6).
pub fn build_thread(branch: &str) -> ThreadRef {
    ThreadRef {
        source: SOURCE.to_owned(),
        id: format!("build/{branch}"),
    }
}

/// Whether `token` has the shape of a ticket's display name: `t`, at least three digits, `_` and a
/// short name. Numbers are padded to three digits and grow past `t999` (`tickets::display_name`).
pub fn is_ticket_name(token: &str) -> bool {
    let Some(rest) = token.strip_prefix('t') else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let Some(name) = rest[digits..].strip_prefix('_') else {
        return false;
    };
    digits >= 3
        && !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// The ticket names in a text, in order, without repeats.
pub fn ticket_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for token in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        if is_ticket_name(token) && !names.iter().any(|n| n == token) {
            names.push(token.to_owned());
        }
    }
    names
}

/// The ticket whose own Thread this is (`ticket/<id>`), if it is one.
pub fn own_ticket_of(reference: &ThreadRef) -> Option<&str> {
    reference.id.strip_prefix("ticket/")
}

/// Whether the text starts with a ticket name followed by a separator or nothing (R1).
pub fn leading_ticket_name(text: &str) -> Option<&str> {
    let trimmed = text.trim_start();
    let end = trimmed
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(trimmed.len());
    let head = &trimmed[..end];
    if !is_ticket_name(head) {
        return None;
    }
    let rest = &trimmed[end..];
    let separated =
        rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace() || c == ':' || c == '：');
    separated.then_some(head)
}

/// Where a message belongs. `ticket_of` resolves a display name to a ticket id.
pub fn place(
    spoken: &Spoken,
    text: &str,
    trigger_id: &str,
    ticket_of: impl Fn(&str) -> Option<String>,
) -> Placement {
    let names = ticket_names(text);
    let (thread, own) = match spoken {
        // R2: spoken to the worker, it is the ticket's whatever it says.
        Spoken::Worker { ticket_id } => (ticket_thread(ticket_id), Some(ticket_id.clone())),
        Spoken::Pm { .. } => match leading_ticket_name(text).and_then(&ticket_of) {
            // R1 and R5: addressed to a ticket by name.
            Some(ticket_id) => (ticket_thread(&ticket_id), Some(ticket_id)),
            // R3: a request of its own.
            None => (request_thread(trigger_id), None),
        },
    };
    // R4: the other tickets named are context, not routes.
    let mentions = names
        .iter()
        .filter_map(|name| ticket_of(name))
        .filter(|id| own.as_ref() != Some(id))
        .collect();
    Placement { thread, mentions }
}

