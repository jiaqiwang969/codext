use super::rename::RenameState;
use super::tests::local_db_first_state;
use super::tests::make_row;
use super::*;
use pretty_assertions::assert_eq;

#[test]
fn rename_updates_only_the_selected_backend() {
    let (mut state, _) = local_db_first_state();
    let backend = state.backend_id;
    let other_backend = crate::backend_id::BackendId::new();
    let thread_id = ThreadId::new();
    let mut original = make_row("/tmp/session.jsonl", "2026-11-05T10:00:00Z", "Original");
    original.thread_id = Some(thread_id);
    original.backend_id = Some(backend);
    let mut other = original.clone();
    other.backend_id = Some(other_backend);
    state.all_rows = vec![original, other];
    state.apply_filter();
    state.rename_state = RenameState::Saving {
        backend_id: backend,
        thread_id,
        name: "Renamed".to_owned(),
    };

    state.handle_rename_result(backend, thread_id, "Renamed".to_owned(), Ok(()));

    assert_eq!(
        state
            .all_rows
            .iter()
            .map(|row| (row.backend_id, row.thread_name.clone()))
            .collect::<Vec<_>>(),
        vec![
            (Some(backend), Some("Renamed".to_owned())),
            (Some(other_backend), None)
        ]
    );
    assert_eq!(state.rename_state, RenameState::Idle);
}

#[test]
fn rename_editor_shows_the_draft_and_rejected_name() {
    let (mut state, _) = local_db_first_state();
    let thread_id = ThreadId::new();
    state.rename_state = RenameState::Editing {
        backend_id: state.backend_id,
        thread_id,
        draft: "Candidate review".to_owned(),
    };
    insta::assert_snapshot!(
        "rename_draft",
        search_line(&state, /*width*/ 80).to_string()
    );
    state.rename_state = RenameState::Editing {
        backend_id: state.backend_id,
        thread_id,
        draft: String::new(),
    };
    state.handle_rename_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    insta::assert_snapshot!(
        "rename_empty_error",
        search_line(&state, /*width*/ 80).to_string()
    );
}
