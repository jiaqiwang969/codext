//! The engine keeps Codext's own automatic drivers off the threads it drives.

use std::sync::Arc;

use codex_extension_api::AutomaticTurnInput;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_superagent_extension::SuperagentService;
use codex_superagent_extension::install;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

const PM: &str = "019a0000-0000-7000-8000-00000000a001";
const OTHER: &str = "019a0000-0000-7000-8000-00000000b002";

#[tokio::test]
async fn declines_goal_and_daemon_continuations_on_engine_threads_only() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let sqlite = SqliteConfig::new_for_testing(dir.path().abs());
    let state = StateRuntime::init(sqlite, "test-provider".to_owned()).await?;
    let service = Arc::new(SuperagentService::new(state, dir.path().join("superagent")));
    service.pm_threads().register(PM)?;
    let mut builder = ExtensionRegistryBuilder::<()>::new();
    install(&mut builder, Arc::clone(&service));
    let registry = builder.build();

    let mut admitted = Vec::new();
    for thread in [PM, OTHER] {
        let store = ExtensionData::new(thread);
        for trigger in [Some("goal"), Some("daemon_recovery"), Some("queue"), None] {
            let admit = registry
                .admit_automatic_turn(AutomaticTurnInput {
                    thread_store: &store,
                    turn_trigger: trigger,
                })
                .await;
            admitted.push((thread, trigger, admit));
        }
    }
    assert_eq!(
        admitted,
        vec![
            (PM, Some("goal"), false),
            (PM, Some("daemon_recovery"), false),
            (PM, Some("queue"), true),
            (PM, None, true),
            (OTHER, Some("goal"), true),
            (OTHER, Some("daemon_recovery"), true),
            (OTHER, Some("queue"), true),
            (OTHER, None, true),
        ]
    );
    Ok(())
}
