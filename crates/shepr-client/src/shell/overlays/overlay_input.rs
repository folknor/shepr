//! Overlay input: rename fields, help and navigator search, menus. Typed text
//! and pasted text land in these editors; input content must stay out of logs
//! and error messages here (log lengths or content-free kinds instead).

use crate::shell::endpoints::ClientEndpointFocusTarget;
use crate::shell::ledger::Work;
use crate::shell::overlays::text_editor::TextEditor;
use crate::shell::state::{
    ClientNavigatorFilter, ClientNavigatorTarget, ClientRenameTarget, ClientShellMode,
    ClientShellOverlay,
};
use crossterm::event::KeyCode;

use crate::shell::state::{
    ClientConfirmCloseOverlay, ClientHelpOverlay, ClientNavigatorOverlay, ClientRenameOverlay,
    ClientShellInput, ClientShellState,
};

impl ClientShellState {
    pub(in crate::shell) fn open_navigator_overlay(&mut self) {
        let mut navigator = ClientNavigatorOverlay {
            query: TextEditor::default(),
            search_focused: false,
            selected: None,
            scroll: 0,
            filter: None,
        };
        let rows = self
            .navigator_index
            .rows(&self.active_endpoint_id, &navigator);
        navigator.selected = rows
            .iter()
            .find(|row| row.current)
            .map(|row| row.target.clone());
        self.overlay = Some(ClientShellOverlay::Navigator(navigator));
    }

    pub(in crate::shell) fn move_navigator_selection(&mut self, delta: isize) {
        let Some(ClientShellOverlay::Navigator(navigator)) = self.overlay.as_mut() else {
            return;
        };
        let rows = self
            .navigator_index
            .rows(&self.active_endpoint_id, navigator);
        if rows.is_empty() {
            navigator.selected = None;
            return;
        }
        let selected = crate::shell::navigation::aggregate_navigation::navigator_selected_index(
            &rows, navigator,
        )
        .unwrap_or(0);
        let max_index = rows.len().saturating_sub(1);
        let next = selected
            .checked_add_signed(delta)
            .unwrap_or(0)
            .min(max_index);
        navigator.selected = Some(rows[next].target.clone());
    }

    pub(in crate::shell) fn scroll_navigator_to(&mut self, scroll: usize, viewport_rows: usize) {
        let Some(ClientShellOverlay::Navigator(navigator)) = self.overlay.as_mut() else {
            return;
        };
        let rows = self
            .navigator_index
            .rows(&self.active_endpoint_id, navigator);
        let viewport_rows = viewport_rows.max(1);
        navigator.scroll = scroll.min(rows.len().saturating_sub(viewport_rows));
        let selected = crate::shell::navigation::aggregate_navigation::navigator_selected_index(
            &rows, navigator,
        )
        .unwrap_or(0);
        // Keep the selection in the dragged viewport so rendering does not snap back to it.
        let selected = selected.clamp(navigator.scroll, navigator.scroll + viewport_rows - 1);
        navigator.selected = rows.get(selected).map(|row| row.target.clone());
    }

    fn move_navigator_workspace(&mut self, forward: bool) {
        let Some(ClientShellOverlay::Navigator(navigator)) = self.overlay.as_mut() else {
            return;
        };
        let rows = self
            .navigator_index
            .rows(&self.active_endpoint_id, navigator);
        let Some(selected) =
            crate::shell::navigation::aggregate_navigation::navigator_selected_index(
                &rows, navigator,
            )
        else {
            return;
        };
        let section = rows[..=selected]
            .iter()
            .rposition(|row| !matches!(row.target, ClientNavigatorTarget::Pane { .. }))
            .unwrap_or(selected);
        let mut destinations = rows.windows(2).enumerate().filter(|(index, pair)| {
            matches!(pair[0].target, ClientNavigatorTarget::Workspace { .. })
                && matches!(pair[1].target, ClientNavigatorTarget::Pane { .. })
                && if forward {
                    *index > section
                } else {
                    *index < section
                }
        });
        let destination = if forward {
            destinations.next()
        } else {
            destinations.next_back()
        };
        if let Some((_, pair)) = destination {
            navigator.selected = Some(pair[1].target.clone());
        }
    }

    pub(in crate::shell) fn accept_navigator_selection(&mut self, outcome: &mut ClientShellInput) {
        let target = self.overlay.as_ref().and_then(|overlay| match overlay {
            ClientShellOverlay::Navigator(navigator) => {
                let rows = self
                    .navigator_index
                    .rows(&self.active_endpoint_id, navigator);
                crate::shell::navigation::aggregate_navigation::selected_navigator_target(
                    &rows, navigator,
                )
            }
            _ => None,
        });
        let Some(target) = target else {
            return;
        };
        let activated = match target {
            ClientNavigatorTarget::Machine { endpoint_id } => {
                self.activate_endpoint(endpoint_id, outcome)
            }
            ClientNavigatorTarget::Workspace {
                endpoint_id,
                workspace_id,
            } => self.focus_or_activate(
                endpoint_id,
                ClientEndpointFocusTarget::Workspace(workspace_id),
                outcome,
            ),
            ClientNavigatorTarget::Pane {
                endpoint_id,
                pane_id,
            } => self.focus_or_activate(
                endpoint_id,
                ClientEndpointFocusTarget::Pane(pane_id),
                outcome,
            ),
        };
        if activated {
            self.overlay = None;
        }
        outcome.repaint = true;
    }

    pub(in crate::shell) fn workspace_action_id(&self) -> Option<shepr_protocol::WorkspaceId> {
        self.navigate_workspace_id
            .as_ref()
            .filter(|target| {
                target.endpoint_id == self.active_endpoint_id
                    && self.navigation_target_valid(target)
            })
            .map(|target| target.workspace_id.clone())
            .or_else(|| {
                self.snapshot
                    .as_deref()
                    .and_then(|snapshot| snapshot.focused_workspace_id.clone())
            })
    }

    /// Opens the new-workspace name prompt with the path-based label and asks
    /// the active endpoint's server, local or remote, for the cwd's checkout
    /// root; the answer replaces the suggestion unless the user has edited it.
    pub(in crate::shell) fn open_new_workspace_overlay(&mut self, outcome: &mut ClientShellInput) {
        let source_workspace_id = self.workspace_action_id();
        let cwd = self.snapshot.as_deref().and_then(|snapshot| {
            let workspace_id = source_workspace_id.as_ref()?;
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == *workspace_id)
                .map(|workspace| workspace.new_workspace_cwd.clone())
        });
        let suggested_name = match cwd.as_deref() {
            Some(cwd) => shepr_core::workspace_label::workspace_label_from_cwd(
                std::path::Path::new(cwd),
                None,
                None,
            ),
            None => "workspace".to_owned(),
        };
        let mut label_lookup = None;
        if let Some(cwd) = cwd.as_deref()
            && self.endpoint_is_online(&self.active_endpoint_id)
        {
            label_lookup = self.submit(
                shepr_protocol::command::EndpointCommand::WorkspaceCheckoutRoot(
                    shepr_protocol::command::WorkspaceCheckoutRootParams {
                        cwd: cwd.to_owned(),
                    },
                ),
                Work::WorkspaceLabel,
                outcome,
            );
        }
        self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            title: "new workspace",
            input: TextEditor::new(&suggested_name, true),
            target: ClientRenameTarget::NewWorkspace {
                cwd,
                suggested_name,
                label_lookup,
            },
        }));
    }

    /// Applies the answer to a `workspace.checkout_root` request. An answer for
    /// an overlay that is gone or was reopened since is ignored, and a failed
    /// lookup keeps the path-based suggestion. Returns whether to repaint.
    pub(in crate::shell) fn complete_workspace_label_lookup(
        &mut self,
        request: &shepr_protocol::RequestId,
        result: Option<shepr_protocol::command::EndpointReply>,
    ) -> bool {
        let Some(ClientShellOverlay::Rename(rename)) = self.overlay.as_mut() else {
            return false;
        };
        let ClientRenameTarget::NewWorkspace {
            cwd,
            suggested_name,
            label_lookup,
            ..
        } = &mut rename.target
        else {
            return false;
        };
        if label_lookup.as_ref() != Some(request) {
            return false;
        }
        *label_lookup = None;
        let Some(shepr_protocol::command::EndpointReply::WorkspaceCheckoutRoot { root, home }) =
            result
        else {
            return false;
        };
        let Some(cwd) = cwd.as_deref() else {
            return false;
        };
        // Only a cwd outside Git can be labelled `~`.
        let home = if root.is_none() { home } else { None };
        let label = shepr_core::workspace_label::workspace_label_from_cwd(
            std::path::Path::new(cwd),
            root.as_deref().map(std::path::Path::new),
            home.as_deref().map(std::path::Path::new),
        );
        if rename.input.as_str() == suggested_name.as_str() {
            rename.input = TextEditor::new(&label, true);
        }
        *suggested_name = label;
        true
    }

    pub(in crate::shell) fn open_rename_workspace_overlay(&mut self) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let Some(workspace_id) = self.workspace_action_id() else {
            return;
        };
        let Some(workspace) = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == workspace_id)
        else {
            return;
        };
        self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            title: "rename workspace",
            input: TextEditor::new(&workspace.label, false),
            target: ClientRenameTarget::Workspace { workspace_id },
        }));
    }

    pub(in crate::shell) fn open_rename_pane_overlay(&mut self) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let Some(pane_id) = snapshot.focused_pane_id.as_deref() else {
            return;
        };
        let Some(pane) = snapshot.panes.iter().find(|pane| pane.pane_id == pane_id) else {
            return;
        };
        self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            title: "rename pane",
            input: TextEditor::new(
                pane.label.as_deref().unwrap_or_default(),
                pane.label.is_none(),
            ),
            target: ClientRenameTarget::Pane {
                pane_id: pane.pane_id.clone(),
            },
        }));
    }

    pub(in crate::shell) fn insert_overlay_text(&mut self, text: &str) -> bool {
        match self.overlay.as_mut() {
            Some(ClientShellOverlay::Rename(rename)) => {
                rename.input.insert(text);
                true
            }
            Some(ClientShellOverlay::Help(help)) if help.search_focused => {
                if help.query.insert(text) {
                    help.scroll = 0;
                }
                true
            }
            Some(ClientShellOverlay::Navigator(navigator)) if navigator.search_focused => {
                if navigator.query.insert(text) {
                    navigator.filter = None;
                    navigator.selected = None;
                }
                true
            }
            _ => false,
        }
    }

    pub(in crate::shell) fn route_overlay_key(
        &mut self,
        key: &shepr_termio::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        use crossterm::event::KeyModifiers;

        if matches!(self.overlay, Some(ClientShellOverlay::GlobalMenu(_))) {
            match key.code {
                KeyCode::Esc => {
                    self.overlay = None;
                    outcome.repaint = true;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.move_global_menu_selection(-1);
                    outcome.repaint = true;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.move_global_menu_selection(1);
                    outcome.repaint = true;
                }
                KeyCode::Enter => {
                    let highlighted = match self.overlay.as_ref() {
                        Some(ClientShellOverlay::GlobalMenu(menu)) => menu.highlighted,
                        _ => return,
                    };
                    self.activate_global_menu_item(highlighted, outcome);
                }
                _ => {}
            }
            return;
        }

        if matches!(self.overlay, Some(ClientShellOverlay::ContextMenu(_))) {
            match key.code {
                KeyCode::Esc => {
                    self.overlay = None;
                    outcome.repaint = true;
                }
                KeyCode::Up => {
                    self.move_context_menu_selection(-1);
                    outcome.repaint = true;
                }
                KeyCode::Down => {
                    self.move_context_menu_selection(1);
                    outcome.repaint = true;
                }
                KeyCode::Enter => {
                    let highlighted = match self.overlay.as_ref() {
                        Some(ClientShellOverlay::ContextMenu(menu)) => menu.highlighted,
                        _ => return,
                    };
                    self.activate_context_menu_item(highlighted, outcome);
                }
                _ => {}
            }
            return;
        }

        if matches!(self.overlay, Some(ClientShellOverlay::Navigator(_))) {
            let (code, modifiers) = shepr_config::normalize_key_combo((key.code, key.modifiers));
            let search_focused = matches!(
                self.overlay,
                Some(ClientShellOverlay::Navigator(ClientNavigatorOverlay {
                    search_focused: true,
                    ..
                }))
            );
            if code == KeyCode::Esc {
                if search_focused {
                    if let Some(ClientShellOverlay::Navigator(navigator)) = self.overlay.as_mut() {
                        navigator.search_focused = false;
                    }
                } else {
                    self.overlay = None;
                }
                outcome.repaint = true;
                return;
            }
            if code == KeyCode::Enter {
                self.accept_navigator_selection(outcome);
                return;
            }
            if search_focused {
                if let Some(ClientShellOverlay::Navigator(navigator)) = self.overlay.as_mut()
                    && let Some(content_changed) = navigator.query.handle_key(key)
                {
                    if content_changed {
                        navigator.filter = None;
                        navigator.selected = None;
                    }
                    outcome.repaint = true;
                    return;
                }
                if code == KeyCode::Up
                    || code == KeyCode::Char('p') && modifiers == KeyModifiers::CONTROL
                {
                    self.move_navigator_selection(-1);
                    outcome.repaint = true;
                    return;
                }
                if code == KeyCode::Down
                    || code == KeyCode::Char('n') && modifiers == KeyModifiers::CONTROL
                {
                    self.move_navigator_selection(1);
                    outcome.repaint = true;
                    return;
                }
                return;
            }
            if matches!(code, KeyCode::Left | KeyCode::Right) && modifiers.is_empty() {
                self.move_navigator_workspace(code == KeyCode::Right);
                outcome.repaint = true;
                return;
            }
            if code == KeyCode::Backspace && modifiers.is_empty() {
                if let Some(ClientShellOverlay::Navigator(navigator)) = self.overlay.as_mut()
                    && navigator.filter.take().is_some()
                {
                    navigator.selected = None;
                }
                outcome.repaint = true;
                return;
            }
            if code == KeyCode::Home && modifiers.is_empty() {
                if let Some(ClientShellOverlay::Navigator(navigator)) = self.overlay.as_mut() {
                    navigator.selected = None;
                    navigator.scroll = 0;
                }
                outcome.repaint = true;
                return;
            }
            if matches!(code, KeyCode::End | KeyCode::Char('G')) && modifiers.is_empty() {
                let last = self.overlay.as_ref().and_then(|overlay| match overlay {
                    ClientShellOverlay::Navigator(navigator) => self
                        .navigator_index
                        .rows(&self.active_endpoint_id, navigator)
                        .last()
                        .map(|row| row.target.clone()),
                    _ => None,
                });
                if let Some(ClientShellOverlay::Navigator(navigator)) = self.overlay.as_mut() {
                    navigator.selected = last;
                }
                outcome.repaint = true;
                return;
            }
            if code == KeyCode::Char('/') && modifiers.is_empty() {
                if let Some(ClientShellOverlay::Navigator(navigator)) = self.overlay.as_mut() {
                    navigator.search_focused = true;
                    navigator.filter = None;
                }
                outcome.repaint = true;
                return;
            }
            if matches!(code, KeyCode::Down | KeyCode::Char('j')) && modifiers.is_empty() {
                self.move_navigator_selection(1);
                outcome.repaint = true;
                return;
            }
            if matches!(code, KeyCode::Up | KeyCode::Char('k')) && modifiers.is_empty() {
                self.move_navigator_selection(-1);
                outcome.repaint = true;
                return;
            }
            if code == KeyCode::Char('d') && modifiers.contains(KeyModifiers::CONTROL) {
                self.move_navigator_selection(8);
                outcome.repaint = true;
                return;
            }
            if code == KeyCode::Char('u') && modifiers.contains(KeyModifiers::CONTROL) {
                self.move_navigator_selection(-8);
                outcome.repaint = true;
                return;
            }
            if let Some(filter) = match code {
                KeyCode::Char('b') if modifiers.is_empty() => Some(ClientNavigatorFilter::Blocked),
                KeyCode::Char('w') if modifiers.is_empty() => Some(ClientNavigatorFilter::Working),
                KeyCode::Char('i') if modifiers.is_empty() => Some(ClientNavigatorFilter::Idle),
                _ => None,
            } {
                if let Some(ClientShellOverlay::Navigator(navigator)) = self.overlay.as_mut() {
                    navigator.query.clear();
                    navigator.filter = Some(filter);
                    navigator.selected = None;
                }
                outcome.repaint = true;
                return;
            }
            if code == KeyCode::Char('a') && modifiers.is_empty() {
                if let Some(ClientShellOverlay::Navigator(navigator)) = self.overlay.as_mut() {
                    navigator.query.clear();
                    navigator.filter = None;
                    navigator.selected = None;
                }
                outcome.repaint = true;
                return;
            }
            return;
        }

        if matches!(self.overlay, Some(ClientShellOverlay::Help(_))) {
            let text_character = shepr_termio::input::keybind_help_text_char(key);
            let (code, modifiers) = shepr_config::normalize_key_combo((key.code, key.modifiers));
            let search_focused = matches!(
                self.overlay,
                Some(ClientShellOverlay::Help(ClientHelpOverlay {
                    search_focused: true,
                    ..
                }))
            );
            if search_focused {
                if let Some(ClientShellOverlay::Help(help)) = self.overlay.as_mut()
                    && let Some(content_changed) = help.query.handle_key(key)
                {
                    if content_changed {
                        help.scroll = 0;
                    }
                    outcome.repaint = true;
                    return;
                }
                match code {
                    KeyCode::Esc => {
                        if let Some(ClientShellOverlay::Help(help)) = self.overlay.as_mut() {
                            help.search_focused = false;
                            help.query.clear();
                            help.scroll = 0;
                        }
                    }
                    KeyCode::Enter => self.overlay = None,
                    KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::PageUp
                    | KeyCode::PageDown
                    | KeyCode::Char('n' | 'p')
                        if !matches!(code, KeyCode::Char(_))
                            || modifiers == KeyModifiers::CONTROL =>
                    {
                        let delta = match code {
                            KeyCode::Up | KeyCode::Char('p') => -1,
                            KeyCode::Down | KeyCode::Char('n') => 1,
                            KeyCode::PageUp => -8,
                            KeyCode::PageDown => 8,
                            _ => unreachable!(),
                        };
                        if let Some(ClientShellOverlay::Help(help)) = self.overlay.as_mut() {
                            help.scroll = help
                                .scroll
                                .saturating_add_signed(delta)
                                .min(self.hits.help_max_scroll);
                        }
                    }
                    _ => {}
                }
                outcome.repaint = true;
                return;
            }

            match code {
                KeyCode::Esc | KeyCode::Enter => self.overlay = None,
                KeyCode::Home => {
                    if let Some(ClientShellOverlay::Help(help)) = self.overlay.as_mut() {
                        help.scroll = 0;
                    }
                }
                KeyCode::End => {
                    if let Some(ClientShellOverlay::Help(help)) = self.overlay.as_mut() {
                        help.scroll = self.hits.help_max_scroll;
                    }
                }
                KeyCode::Up
                | KeyCode::Down
                | KeyCode::Char('k' | 'j')
                | KeyCode::PageUp
                | KeyCode::PageDown => {
                    let delta = match code {
                        KeyCode::Up | KeyCode::Char('k') => -1,
                        KeyCode::Down | KeyCode::Char('j') => 1,
                        KeyCode::PageUp => -8,
                        KeyCode::PageDown => 8,
                        _ => unreachable!(),
                    };
                    if let Some(ClientShellOverlay::Help(help)) = self.overlay.as_mut() {
                        help.scroll = help
                            .scroll
                            .saturating_add_signed(delta)
                            .min(self.hits.help_max_scroll);
                    }
                }
                _ if text_character == Some('/') => {
                    if let Some(ClientShellOverlay::Help(help)) = self.overlay.as_mut() {
                        help.search_focused = true;
                        help.scroll = 0;
                    }
                }
                _ if text_character == Some('?') => self.overlay = None,
                _ => {}
            }
            outcome.repaint = true;
            return;
        }

        if matches!(self.overlay, Some(ClientShellOverlay::ConfirmClose(_))) {
            if key.code == KeyCode::Enter {
                self.accept_close_confirmation(outcome);
            } else if key.code == KeyCode::Esc {
                let return_to_navigate = matches!(
                    self.overlay.take(),
                    Some(ClientShellOverlay::ConfirmClose(
                        ClientConfirmCloseOverlay {
                            return_to_navigate: true,
                            ..
                        }
                    ))
                );
                if return_to_navigate {
                    self.mode = ClientShellMode::Navigate;
                    self.navigate_workspace_id = self.focused_navigation_target();
                    self.reveal_navigation_workspace = true;
                }
                outcome.repaint = true;
            }
            return;
        }

        let Some(ClientShellOverlay::Rename(rename)) = self.overlay.as_mut() else {
            return;
        };
        if key.code == KeyCode::Enter {
            self.save_rename_overlay(outcome);
            return;
        }
        if key.code == KeyCode::Esc {
            self.overlay = None;
            outcome.repaint = true;
            return;
        }
        if key
            .generated_text
            .as_deref()
            .is_some_and(|text| !text.is_empty())
        {
            outcome.repaint |= rename.input.handle_key(key).is_some();
            return;
        }
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            rename.input.clear();
            outcome.repaint = true;
            return;
        }
        if key.code == KeyCode::Backspace && key.modifiers.contains(KeyModifiers::SUPER) {
            rename.input.clear();
            outcome.repaint = true;
            return;
        }
        if rename.input.handle_key(key).is_some() {
            outcome.repaint = true;
        }
    }

    pub(in crate::shell) fn save_rename_overlay(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::Rename(rename)) = self.overlay.take() else {
            return;
        };
        let trimmed = rename.input.trim();
        // An empty rename clears the custom name, the same as the context
        // menu's Clear, so the automatic label returns.
        let label = (!trimmed.is_empty()).then(|| trimmed.to_owned());
        let command = match rename.target {
            ClientRenameTarget::NewWorkspace {
                cwd,
                suggested_name,
                ..
            } => shepr_protocol::command::EndpointCommand::WorkspaceCreate(
                shepr_protocol::command::WorkspaceCreateParams {
                    // The prompt already resolved the directory the new
                    // workspace starts in; with none known the server picks.
                    source: match cwd {
                        Some(cwd) => shepr_protocol::command::WorkspaceCreateSource::Cwd(cwd),
                        None => shepr_protocol::command::WorkspaceCreateSource::Default,
                    },
                    label: label.filter(|label| *label != suggested_name),
                },
            ),
            ClientRenameTarget::Workspace { workspace_id } => {
                shepr_protocol::command::EndpointCommand::WorkspaceRename(
                    shepr_protocol::command::WorkspaceRenameParams {
                        workspace_id,
                        label,
                    },
                )
            }
            ClientRenameTarget::Pane { pane_id } => {
                shepr_protocol::command::EndpointCommand::PaneRename(
                    shepr_protocol::command::PaneRenameParams { pane_id, label },
                )
            }
        };
        self.push_endpoint_command(command, outcome);
        outcome.repaint = true;
    }

    pub(in crate::shell) fn accept_close_confirmation(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::ConfirmClose(confirm)) = self.overlay.take() else {
            return;
        };
        outcome.repaint = true;
        self.push_endpoint_command(
            shepr_protocol::command::EndpointCommand::WorkspaceClose(
                shepr_protocol::command::WorkspaceCloseParams {
                    workspace_id: confirm.workspace_id,
                },
            ),
            outcome,
        );
    }

    pub(in crate::shell) fn open_confirm_close_overlay(
        &mut self,
        workspace_id: shepr_protocol::WorkspaceId,
    ) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let Some(workspace) = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == workspace_id)
        else {
            return;
        };
        let pane_count = snapshot
            .panes
            .iter()
            .filter(|pane| pane.workspace_id == workspace.workspace_id)
            .count();
        let scope = if pane_count == 1 {
            "1 pane".to_owned()
        } else {
            format!("{pane_count} panes")
        };
        self.overlay = Some(ClientShellOverlay::ConfirmClose(
            ClientConfirmCloseOverlay {
                workspace_id,
                title: "Close workspace?".to_owned(),
                detail: format!("{} - {scope}", workspace.label),
                return_to_navigate: self.mode == ClientShellMode::Navigate,
            },
        ));
    }
}
