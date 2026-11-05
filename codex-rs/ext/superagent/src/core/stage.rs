//! Ticket stage classification and retry eligibility (prototype `src/core/stage.ts`, docs/003).

use serde::Serialize;

use super::model::AssignmentState;
use super::model::ReportState;
use super::model::Ticket;
use super::model::latest_assignment;
use super::model::waits_for_person;

/// High-level execution stage of a ticket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "stage", rename_all = "lowercase")]
pub enum TicketStage {
    Person {
        action: PersonAction,
    },
    Working {
        #[serde(skip_serializing_if = "Option::is_none")]
        action: Option<WorkingAction>,
    },
    Idle,
    Ended,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PersonAction {
    Delivery,
    Answer,
    Review,
    Blocked,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkingAction {
    Queued,
}

impl TicketStage {
    /// The closed card state, such as `person/review` or `working/queued`.
    pub fn card(&self) -> String {
        match self {
            Self::Person { action } => format!("person/{}", action.name()),
            Self::Working { action: Some(_) } => "working/queued".to_owned(),
            Self::Working { action: None } => "working".to_owned(),
            Self::Idle => "idle".to_owned(),
            Self::Ended => "ended".to_owned(),
        }
    }
}

impl PersonAction {
    fn name(self) -> &'static str {
        match self {
            Self::Delivery => "delivery",
            Self::Answer => "answer",
            Self::Review => "review",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
        }
    }
}

/// Classifies a ticket's stage; the rules follow the table in docs/003 in order, first match wins.
pub fn ticket_stage(ticket: &Ticket) -> TicketStage {
    if ticket.ended.is_some() {
        return TicketStage::Ended;
    }
    if ticket.delivery.is_some() {
        return TicketStage::Person {
            action: PersonAction::Delivery,
        };
    }
    if ticket.attention.is_some() {
        return TicketStage::Person {
            action: PersonAction::Answer,
        };
    }
    let assignment = latest_assignment(ticket);
    let report = assignment.and_then(|a| a.report.as_ref()).map(|r| r.state);
    if report == Some(ReportState::Ready) {
        return TicketStage::Person {
            action: PersonAction::Review,
        };
    }
    match report {
        Some(ReportState::Blocked) if waits_for_person(ticket) => {
            return TicketStage::Person {
                action: PersonAction::Blocked,
            };
        }
        Some(ReportState::Failed) if waits_for_person(ticket) => {
            return TicketStage::Person {
                action: PersonAction::Failed,
            };
        }
        _ => {}
    }
    match assignment {
        None => TicketStage::Working {
            action: Some(WorkingAction::Queued),
        },
        Some(a) if a.state == AssignmentState::Pending => TicketStage::Working {
            action: Some(WorkingAction::Queued),
        },
        Some(a) if report == Some(ReportState::Waiting) || a.state == AssignmentState::Ended => {
            TicketStage::Idle
        }
        Some(_) => TicketStage::Working { action: None },
    }
}

/// An open ticket stalled on a blocked or failed report without a newer human follow-up, or with a
/// failed Input delivery.
pub fn can_retry(ticket: &Ticket) -> bool {
    ticket.ended.is_none() && (waits_for_person(ticket) || ticket.delivery.is_some())
}
