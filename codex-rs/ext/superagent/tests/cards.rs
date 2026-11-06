//! The cards: bounded projections the models read (ported from the shape cases of the prototype's
//! `tests/cards.spec.ts` and the relay cases of `tests/tools.spec.ts`).

mod common;

use codex_superagent_extension::cards::CandidateContext;
use codex_superagent_extension::cards::RELATED_TICKETS;
use codex_superagent_extension::cards::SUMMARY_CHARS;
use codex_superagent_extension::cards::TICKET_STATES;
use codex_superagent_extension::cards::candidate_card;
use codex_superagent_extension::cards::next_ticket_number;
use codex_superagent_extension::cards::preview;
use codex_superagent_extension::cards::related_tickets;
use codex_superagent_extension::cards::text_page;
use codex_superagent_extension::cards::ticket_card;
use codex_superagent_extension::cards::worker_attention_relay;
use codex_superagent_extension::cards::worker_report_relay;
use codex_superagent_extension::core::AssignmentState;
use codex_superagent_extension::core::Attention;
use codex_superagent_extension::core::Report;
use codex_superagent_extension::core::ReportState;
use codex_superagent_extension::core::Strength;
use codex_superagent_extension::core::TriggerDisposition;
use common::ticket;
use common::trigger;
use pretty_assertions::assert_eq;
use serde_json::Value;

#[test]
fn previews_collapse_whitespace_and_stop_at_160_characters() {
    let text = format!("one  two\n\nthree {}", "x".repeat(300));
    let shown = preview(&text);
    assert!(shown.starts_with("one two three x"));
    assert_eq!(shown.chars().count(), 160);
    assert_eq!(preview("短  句"), "短 句");
}

#[test]
fn a_ticket_card_shows_a_closed_state_and_the_question_it_waits_on() {
    let mut open = ticket("t1", false, &[AssignmentState::Running], &[]);
    assert_eq!(ticket_card(&open)["state"], "working");
    open.assignments[0].report = Some(Report {
        state: ReportState::Ready,
        summary: "done".into(),
        at: "1".into(),
    });
    assert_eq!(ticket_card(&open)["state"], "person/review");
    open.attention = Some(Attention {
        id: "q1".into(),
        question: "which branch?".into(),
        trigger_id: Some("tr".into()),
    });
    let card = ticket_card(&open);
    assert_eq!(card["state"], "person/answer");
    assert_eq!(card["attention"]["question"], "which branch?");
    assert_eq!(card["read"], "ticket/t1");
    let ended = ticket("t2", true, &[AssignmentState::Ended], &[]);
    let card = ticket_card(&ended);
    assert_eq!(card["state"], "closed");
    assert_eq!(card["ended"]["reason"], "closed");
    for state in [open, ended].iter().map(|t| {
        ticket_card(t)["state"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }) {
        assert!(TICKET_STATES.contains(&state.as_str()), "{state}");
    }
}

#[test]
fn text_pages_slice_by_characters_and_point_at_the_next_page() {
    let text = "一二三四五六七八九十".repeat(3);
    let first = text_page("ticket/t1/result", &text, 0, 12);
    assert_eq!(first["text"], "一二三四五六七八九十一二");
    assert_eq!(first["next_offset"], 12);
    assert_eq!(first["total_chars"], 30);
    let last = text_page("ticket/t1/result", &text, 24, 12);
    assert_eq!(last["text"], "五六七八九十");
    assert_eq!(last["next_offset"], Value::Null);
}

#[test]
fn related_tickets_list_open_ones_first_newest_first_and_at_most_eight() {
    let mut tickets = Vec::new();
    for i in 0..10 {
        let mut t = ticket(&format!("t{i}"), i % 2 == 0, &[AssignmentState::Ended], &[]);
        t.created_at = format!("2026-11-{:02}", i + 1);
        tickets.push(t);
    }
    let (cards, total) = related_tickets(&tickets);
    assert_eq!(total, 10);
    assert_eq!(cards.len(), RELATED_TICKETS);
    let ids: Vec<&str> = cards
        .iter()
        .map(|c| c["ticket_id"].as_str().unwrap_or_default())
        .collect();
    // Open tickets (odd ids) come first, newest first.
    assert_eq!(&ids[..5], &["t9", "t7", "t5", "t3", "t1"]);
    assert!(
        ids[5..]
            .iter()
            .all(|id| ["t0", "t2", "t4", "t6", "t8"].contains(id))
    );
}

#[test]
fn a_report_relay_bounds_the_summary_to_600_characters_with_a_read_pointer() {
    let t = ticket("t1", false, &[AssignmentState::Running], &[]);
    let long = "字".repeat(SUMMARY_CHARS + 50);
    let relay = worker_report_relay(&t, "ready", &long, None, None);
    let body: Value = serde_json::from_str(&relay.text).unwrap_or_default();
    assert_eq!(body["kind"], "worker_report");
    assert_eq!(
        body["summary"].as_str().map(|s| s.chars().count()),
        Some(SUMMARY_CHARS)
    );
    assert_eq!(body["chars"], SUMMARY_CHARS + 50);
    assert_eq!(body["read"], "ticket/t1/result");
    assert!(body["ticket"].get("preview").is_none());
    assert!(relay.id.starts_with("report:"));
    let short = worker_report_relay(&t, "ready", "done", Some("tr-q"), None);
    let body: Value = serde_json::from_str(&short.text).unwrap_or_default();
    assert_eq!(body["summary"], "done");
    assert!(body.get("chars").is_none());
    assert_eq!(body["answer_to"], "tr-q");
    let attention = worker_attention_relay(&t, "q1", "progress");
    let body: Value = serde_json::from_str(&attention.text).unwrap_or_default();
    assert_eq!(body["kind"], "worker_attention");
    assert_eq!(attention.id, "attention:t1:q1");
}

#[test]
fn a_candidate_card_bounds_the_original_text_and_lists_where_it_went() {
    let long = format!("请修一下 {}", "x".repeat(700));
    let primary = trigger(
        "TR-7",
        "codext",
        &[("codext", "pm/TR-7")],
        "2026-11-02T00:00:00.000Z",
        &long,
    );
    let earlier = trigger(
        "TR-6",
        "codext",
        &[("codext", "pm/TR-7")],
        "2026-11-01T00:00:00.000Z",
        "earlier words",
    );
    let disposition = TriggerDisposition {
        fanout: vec!["t017_fix".into()],
        at: String::new(),
        ..TriggerDisposition::default()
    };
    let tickets = vec![ticket("t017_fix", false, &[AssignmentState::Running], &[])];
    let batch = [earlier, primary];
    let context = CandidateContext {
        dispositions: vec![(&batch[0], None), (&batch[1], Some(&disposition))],
        referenced: Vec::new(),
        tickets_total: 22,
        mode: "observe",
        origins: vec![],
        origin_texts: Default::default(),
    };
    let card = candidate_card("eval-1", &batch, Strength::Explicit, &tickets, &context);
    let (header, body) = card.split_once('\n').unwrap_or_default();
    assert_eq!(
        header,
        "[候选卡 eval=eval-1] 触发 TR-7 · 强度 明确 · 模式 observe"
    );
    let body: Value = serde_json::from_str(body).unwrap_or_default();
    assert_eq!(body["trigger_id"], "TR-7");
    assert_eq!(
        body["text"].as_str().map(|s| s.chars().count()),
        Some(SUMMARY_CHARS)
    );
    assert_eq!(body["chars"], long.chars().count());
    assert_eq!(body["read"], "trigger/TR-7");
    assert_eq!(body["batch"][0]["trigger_id"], "TR-6");
    assert_eq!(body["delivered_to"], serde_json::json!(["t017_fix"]));
    assert_eq!(body["related_tickets"][0]["ticket_id"], "t017_fix");
    assert_eq!(body["tickets_total"], 1);
    assert_eq!(body["next_ticket_number"], "t023");
    assert_eq!(next_ticket_number(0), "t001");
}
