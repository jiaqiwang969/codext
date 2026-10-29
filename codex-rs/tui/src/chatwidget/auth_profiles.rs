//! TUI picker for account-pool authentication profiles.

use crate::app_event::AppEvent;
use crate::auth_profile::list_auth_profiles;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;

use super::ChatWidget;

impl ChatWidget {
    pub(crate) fn show_auth_profile_picker(&mut self) {
        let profiles = match list_auth_profiles(self.config.codex_home.as_path()) {
            Ok(profiles) => profiles,
            Err(error) => {
                self.add_error_message(format!("Unable to list authentication profiles: {error}"));
                return;
            }
        };

        if profiles.is_empty() {
            self.add_info_message(
                "No authentication profiles found. Add accounts under CODEX_HOME/accounts/ or use the account-pool launcher.".to_string(),
                None,
            );
            return;
        }

        let items = profiles
            .into_iter()
            .map(|name| {
                let action_name = name.clone();
                SelectionItem {
                    name,
                    description: Some("switch the account used by this session".to_string()),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::SwitchAuthProfile {
                            name: action_name.clone(),
                        });
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                }
            })
            .collect();

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Authentication account".to_string()),
            subtitle: Some("Choose an account".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            is_searchable: true,
            search_placeholder: Some("Type to filter accounts...".to_string()),
            ..SelectionViewParams::picker()
        });
    }
}
