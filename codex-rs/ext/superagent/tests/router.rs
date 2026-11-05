//! The Router's level switch and the World State sections it reads: its rules and the index.

use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::input_text;
use codex_superagent_extension::host::SessionBrief;
use codex_superagent_extension::router::MODE_FILE;
use codex_superagent_extension::router::RouterMode;
use codex_superagent_extension::router::index_row;
use codex_superagent_extension::router::index_section;
use codex_superagent_extension::router::rules_section;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

fn text_of(fragment: Option<RenderedWorldStateFragment>) -> Option<String> {
    fragment.map(|fragment| match fragment.body() {
        codex_protocol::models::ContentItem::InputText { text } => text.clone(),
        other => panic!("unexpected body {other:?}"),
    })
}

#[test]
fn only_the_word_drive_turns_routing_on() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let mut seen = vec![RouterMode::read(dir.path())];
    for text in ["drive\n", "observe", "Drive", ""] {
        std::fs::write(dir.path().join(MODE_FILE), text)?;
        seen.push(RouterMode::read(dir.path()));
    }
    assert_eq!(
        seen,
        vec![
            RouterMode::Observe,
            RouterMode::Drive,
            RouterMode::Observe,
            RouterMode::Observe,
            RouterMode::Observe,
        ]
    );
    Ok(())
}

#[test]
fn the_rules_are_told_once_per_level() {
    let observe = rules_section(RouterMode::Observe);
    let (snapshot, first) = observe.render_diff(PreviousWorldStateSection::Absent);
    assert_eq!(snapshot, Some(json!("observe")));
    let first = text_of(first).expect("the rules on the first step");
    assert!(first.contains("只观察"), "{first}");

    let same = Value::String("observe".to_owned());
    let (_, again) = observe.render_diff(PreviousWorldStateSection::Known(&same));
    assert_eq!(again, None);

    let drive = rules_section(RouterMode::Drive);
    let (snapshot, changed) = drive.render_diff(PreviousWorldStateSection::Known(&same));
    assert_eq!(snapshot, Some(json!("drive")));
    assert!(text_of(changed).expect("the new level").contains("推进"));
    assert!(observe.matches_retained_fragment("developer", "<superagent_router>\nx"));
    assert!(!observe.matches_retained_fragment("user", "<superagent_router>\nx"));
}

fn session(id: &str, name: Option<&str>, status: &str) -> SessionBrief {
    SessionBrief {
        id: id.to_owned(),
        name: name.map(str::to_owned),
        preview: "修复 profile 切换\n未生效的问题".to_owned(),
        cwd: "/Users/someone/00-nixos-config/codext".to_owned(),
        status: status.to_owned(),
        updated_at: None,
        account_profile: Some("Simmonsalex805".to_owned()),
    }
}

#[test]
fn a_row_names_the_session_briefly() {
    assert_eq!(
        vec![
            index_row(
                &session("a1", Some("Router 调研"), "idle"),
                "/Users/someone"
            ),
            index_row(&session("b2", Some("  "), "active"), "/Users/someone"),
            index_row(
                &SessionBrief {
                    account_profile: None,
                    ..session("c3", None, "notLoaded")
                },
                "/elsewhere"
            ),
        ],
        vec![
            "a1 | Router 调研 | ~/00-nixos-config/codext | idle | - | Simmonsalex805",
            "b2 | 修复 profile 切换 未生效的问题 | ~/00-nixos-config/codext | active | - | Simmonsalex805",
            "c3 | 修复 profile 切换 未生效的问题 | /Users/someone/00-nixos-config/codext | notLoaded | - | -",
        ]
    );
}

#[test]
fn the_index_is_told_whole_once_then_only_what_changed() {
    let rows = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(id, row)| ((*id).to_owned(), (*row).to_owned()))
            .collect()
    };
    let first = index_section(rows(&[("a1", "a1 | x | idle"), ("b2", "b2 | y | idle")]));
    let (snapshot, full) = first.render_diff(PreviousWorldStateSection::Absent);
    let snapshot = snapshot.expect("a snapshot");
    assert_eq!(
        snapshot,
        json!({ "a1": "a1 | x | idle", "b2": "b2 | y | idle" })
    );
    let full = text_of(full).expect("the full index");
    assert!(full.ends_with("a1 | x | idle\nb2 | y | idle\n"), "{full}");
    assert!(first.matches_retained_fragment("developer", "<superagent_index>\nx"));
    assert!(!first.matches_retained_fragment("developer", "<superagent_index_update>\nx"));

    let (_, unchanged) = first.render_diff(PreviousWorldStateSection::Known(&snapshot));
    assert_eq!(unchanged, None);

    let next = index_section(rows(&[
        ("c3", "c3 | z | active"),
        ("a1", "a1 | x | active"),
    ]));
    let (_, update) = next.render_diff(PreviousWorldStateSection::Known(&snapshot));
    assert_eq!(
        update,
        Some(RenderedWorldStateFragment::new(
            "developer",
            ("<superagent_index_update>", "</superagent_index_update>"),
            input_text(
                "会话索引的变化（列同 <superagent_index>）：\nc3 | z | active\na1 | x | active\n不在最近 30 个之列了：b2\n"
            ),
        ))
    );
}
