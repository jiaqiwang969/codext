//! The Router's level switch and the World State sections it reads: its rules and the index.

use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::input_text;
use codex_superagent_extension::host::SessionBrief;
use codex_superagent_extension::router::MODE_FILE;
use codex_superagent_extension::router::RouterMode;
use codex_superagent_extension::router::attention_of;
use codex_superagent_extension::router::attention_section;
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
fn only_drive_and_org_leave_observing_and_only_org_routes_requests() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let mut seen = vec![RouterMode::read(dir.path())];
    for text in ["drive\n", "org", "observe", "Drive", ""] {
        std::fs::write(dir.path().join(MODE_FILE), text)?;
        seen.push(RouterMode::read(dir.path()));
    }
    assert_eq!(
        seen,
        vec![
            RouterMode::Observe,
            RouterMode::Drive,
            RouterMode::Org,
            RouterMode::Observe,
            RouterMode::Observe,
            RouterMode::Observe,
        ]
    );
    assert_eq!(
        seen.iter()
            .map(|mode| mode.routes_requests())
            .collect::<Vec<_>>(),
        vec![false, false, true, false, false, false]
    );
    Ok(())
}

#[test]
fn the_rules_are_told_once_per_level() {
    let observe = rules_section(RouterMode::Observe);
    let (snapshot, first) = observe.render_diff(PreviousWorldStateSection::Absent);
    let same = snapshot.expect("a snapshot");
    let told = same.as_str().unwrap_or_default().to_owned();
    assert!(told.starts_with("observe:"), "{told}");
    let first = text_of(first).expect("the rules on the first step");
    assert!(first.contains("只观察"), "{first}");

    let (_, again) = observe.render_diff(PreviousWorldStateSection::Known(&same));
    assert_eq!(again, None);
    // Rules of an older text are told again.
    let older = json!("observe:00000000");
    let (_, retold) = observe.render_diff(PreviousWorldStateSection::Known(&older));
    assert!(retold.is_some());

    let drive = rules_section(RouterMode::Drive);
    let (snapshot, changed) = drive.render_diff(PreviousWorldStateSection::Known(&same));
    assert!(
        snapshot
            .as_ref()
            .and_then(Value::as_str)
            .is_some_and(|told| told.starts_with("drive:"))
    );
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
        model: Some("gpt-6-astra".to_owned()),
        effort: Some("high".to_owned()),
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
                    effort: None,
                    account_profile: None,
                    ..session("c3", None, "notLoaded")
                },
                "/elsewhere"
            ),
        ],
        vec![
            "a1 | Router 调研 | ~/00-nixos-config/codext | idle | - | gpt-6-astra/high | Simmonsalex805",
            "b2 | 修复 profile 切换 未生效的问题 | ~/00-nixos-config/codext | active | - | gpt-6-astra/high | Simmonsalex805",
            "c3 | 修复 profile 切换 未生效的问题 | /Users/someone/00-nixos-config/codext | notLoaded | - | gpt-6-astra | -",
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

#[test]
fn each_problem_is_told_when_it_appears_and_when_it_is_resolved() {
    assert_eq!(
        [
            "idle",
            "systemError",
            "active:waitingOnApproval",
            "active:waitingOnUserInput",
            "active"
        ]
        .map(attention_of),
        [None, Some("出错"), Some("等你批准"), Some("等你回答"), None]
    );
    let none = attention_section(Vec::new());
    let (snapshot, fragment) = none.render_diff(PreviousWorldStateSection::Absent);
    assert_eq!((snapshot, fragment), (Some(json!({})), None));

    let first = attention_section(vec![("a1".to_owned(), "等你批准", "a1 | x".to_owned())]);
    let (snapshot, full) = first.render_diff(PreviousWorldStateSection::Absent);
    let snapshot = snapshot.expect("a snapshot");
    assert_eq!(snapshot, json!({ "a1": "等你批准" }));
    assert_eq!(
        text_of(full),
        Some("需要你处理的会话（每个问题只列一次）：\n等你批准：a1 | x\n".to_owned())
    );

    // The same problem on a row that changed (a newer time) is not told again.
    let same = attention_section(vec![("a1".to_owned(), "等你批准", "a1 | x 2".to_owned())]);
    let (_, again) = same.render_diff(PreviousWorldStateSection::Known(&snapshot));
    assert_eq!(again, None);

    let next = attention_section(vec![("b2".to_owned(), "出错", "b2 | y".to_owned())]);
    let (_, update) = next.render_diff(PreviousWorldStateSection::Known(&snapshot));
    assert_eq!(
        update,
        Some(RenderedWorldStateFragment::new(
            "developer",
            (
                "<superagent_attention_update>",
                "</superagent_attention_update>"
            ),
            input_text("需要你处理的会话有变化：\n出错：b2 | y\n已不再等你：a1\n"),
        ))
    );
}
