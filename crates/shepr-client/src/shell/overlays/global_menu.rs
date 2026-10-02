use crate::shell::state::ClientShellOverlay;
use crate::shell::state::{ClientGlobalMenuOverlay, ClientShellInput, ClientShellState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::shell) enum ClientGlobalMenuAction {
    Binding(shepr_termio::input::KeybindAction),
}

pub(in crate::shell) fn global_menu_items() -> Vec<(&'static str, ClientGlobalMenuAction)> {
    vec![
        (
            "keybinds",
            ClientGlobalMenuAction::Binding(shepr_termio::input::KeybindAction::Help),
        ),
        (
            "detach",
            ClientGlobalMenuAction::Binding(shepr_termio::input::KeybindAction::Detach),
        ),
    ]
}

impl ClientShellState {
    pub(in crate::shell) fn toggle_global_menu(&mut self) {
        if matches!(self.overlay, Some(ClientShellOverlay::GlobalMenu(_))) {
            self.overlay = None;
        } else {
            self.overlay = Some(ClientShellOverlay::GlobalMenu(ClientGlobalMenuOverlay {
                highlighted: 0,
                launcher: self.hits.global_launcher,
            }));
        }
    }

    pub(in crate::shell) fn move_global_menu_selection(&mut self, delta: isize) {
        let item_count = global_menu_items().len();
        let Some(ClientShellOverlay::GlobalMenu(menu)) = self.overlay.as_mut() else {
            return;
        };
        let max_index = item_count.saturating_sub(1);
        menu.highlighted = menu
            .highlighted
            .checked_add_signed(delta)
            .unwrap_or(0)
            .min(max_index);
    }

    pub(in crate::shell) fn activate_global_menu_item(
        &mut self,
        index: usize,
        outcome: &mut ClientShellInput,
    ) {
        let Some(action) = global_menu_items().get(index).map(|(_, action)| *action) else {
            return;
        };
        self.overlay = None;
        match action {
            ClientGlobalMenuAction::Binding(binding) => {
                self.record_binding(&shepr_termio::input::KeybindMatch::Action(binding), outcome);
            }
        }
        outcome.repaint = true;
    }
}
