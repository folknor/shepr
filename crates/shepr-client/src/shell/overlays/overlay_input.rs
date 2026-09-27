//! Overlay input: rename fields, help and navigator search, menus. Typed text
//! and pasted text land in these editors; input content must stay out of logs
//! and error messages here (log lengths or content-free kinds instead).

use super::*;

impl ClientShellState {
    pub(super) fn open_navigator_overlay(&mut self) {
        let mut navigator = ClientNavigatorOverlay {
            query: TextEditor::default(),
            search_focused: false,
            selected: None,
            scroll: 0,
            filter: None,
        };
        let rows =
            render::client_navigator_rows(&self.endpoints, &self.active_endpoint_id, &navigator);
        navigator.selected = rows
            .iter()
            .find(|row| row.current)
            .map(|row| row.target.clone());
        self.overlay = Some(ClientShellOverlay::Navigator(navigator));
    }

    pub(super) fn move_navigator_selection(&mut self, delta: isize) {
        let Some(ClientShellOverlay::Navigator(navigator)) = self.overlay.as_mut() else {
            return;
        };
        let rows =
            render::client_navigator_rows(&self.endpoints, &self.active_endpoint_id, navigator);
        if rows.is_empty() {
            navigator.selected = None;
            return;
        }
        let selected =
            super::aggregate_navigation::navigator_selected_index(&rows, navigator).unwrap_or(0);
        let max_index = rows.len().saturating_sub(1);
        let next = selected
            .checked_add_signed(delta)
            .unwrap_or(0)
            .min(max_index);
        navigator.selected = Some(rows[next].target.clone());
    }

    pub(super) fn scroll_navigator_to(&mut self, scroll: usize, viewport_rows: usize) {
        let Some(ClientShellOverlay::Navigator(navigator)) = self.overlay.as_mut() else {
            return;
        };
        let rows =
            render::client_navigator_rows(&self.endpoints, &self.active_endpoint_id, navigator);
        let viewport_rows = viewport_rows.max(1);
        navigator.scroll = scroll.min(rows.len().saturating_sub(viewport_rows));
        let selected =
            super::aggregate_navigation::navigator_selected_index(&rows, navigator).unwrap_or(0);
        // Keep the selection in the dragged viewport so rendering does not snap back to it.
        let selected = selected.clamp(navigator.scroll, navigator.scroll + viewport_rows - 1);
        navigator.selected = rows.get(selected).map(|row| row.target.clone());
    }

    fn move_navigator_workspace(&mut self, forward: bool) {
        let Some(ClientShellOverlay::Navigator(navigator)) = self.overlay.as_mut() else {
            return;
        };
        let rows =
            render::client_navigator_rows(&self.endpoints, &self.active_endpoint_id, navigator);
        let Some(selected) =
            super::aggregate_navigation::navigator_selected_index(&rows, navigator)
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

    pub(super) fn accept_navigator_selection(&mut self, outcome: &mut ClientShellInput) {
        let target = self.overlay.as_ref().and_then(|overlay| match overlay {
            ClientShellOverlay::Navigator(navigator) => {
                let rows = render::client_navigator_rows(
                    &self.endpoints,
                    &self.active_endpoint_id,
                    navigator,
                );
                super::aggregate_navigation::selected_navigator_target(&rows, navigator)
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

    pub(super) fn workspace_action_id(&self) -> Option<String> {
        self.navigate_workspace_id
            .as_ref()
            .filter(|target| {
                target.endpoint_id == self.active_endpoint_id
                    && self.navigation_target_valid(target)
            })
            .map(|target| target.workspace_id.clone())
            .or_else(|| {
                self.snapshot.as_deref().and_then(|snapshot| {
                    snapshot
                        .focused_workspace_id
                        .as_ref()
                        .map(ToString::to_string)
                })
            })
    }

    pub(super) fn open_new_workspace_overlay(&mut self) {
        let source_workspace_id = self.workspace_action_id();
        let cwd = self.snapshot.as_deref().and_then(|snapshot| {
            let workspace_id = source_workspace_id.as_deref()?;
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == workspace_id)
                .map(|workspace| workspace.new_workspace_cwd.clone())
        });
        let suggested_name = cwd
            .as_deref()
            .map(std::path::Path::new)
            .map(crate::workspace_label::derive_label_from_cwd)
            .unwrap_or_else(|| "workspace".to_owned());
        self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            title: "new workspace",
            input: TextEditor::new(&suggested_name, true),
            target: ClientRenameTarget::NewWorkspace {
                source_workspace_id,
                cwd,
                suggested_name,
            },
        }));
    }

    pub(super) fn open_rename_workspace_overlay(&mut self) {
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

    pub(super) fn open_new_tab_overlay(&mut self) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let Some(workspace_id) = snapshot
            .focused_workspace_id
            .as_ref()
            .map(ToString::to_string)
        else {
            return;
        };
        let default_name = (snapshot
            .tabs
            .iter()
            .filter(|tab| tab.workspace_id == workspace_id)
            .count()
            + 1)
        .to_string();
        self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            title: "new tab",
            input: TextEditor::new(&default_name, true),
            target: ClientRenameTarget::NewTab {
                workspace_id,
                default_name,
            },
        }));
    }

    pub(super) fn open_rename_tab_overlay(&mut self) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let Some(tab_id) = snapshot.focused_tab_id.as_deref() else {
            return;
        };
        let Some(tab) = snapshot.tabs.iter().find(|tab| tab.tab_id == tab_id) else {
            return;
        };
        self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            title: "rename tab",
            input: TextEditor::new(&tab.label, false),
            target: ClientRenameTarget::Tab {
                tab_id: tab.tab_id.clone(),
                auto_name: !tab.custom_label,
                original_name: tab.label.clone(),
            },
        }));
    }

    pub(super) fn open_rename_pane_overlay(&mut self) {
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

    pub(super) fn insert_overlay_text(&mut self, text: &str) -> bool {
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

    pub(super) fn route_overlay_key(
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
                    ClientShellOverlay::Navigator(navigator) => render::client_navigator_rows(
                        &self.endpoints,
                        &self.active_endpoint_id,
                        navigator,
                    )
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
                | KeyCode::Char('k')
                | KeyCode::Down
                | KeyCode::Char('j')
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

    pub(super) fn save_rename_overlay(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::Rename(rename)) = self.overlay.take() else {
            return;
        };
        let trimmed = rename.input.trim();
        let method = match rename.target {
            ClientRenameTarget::NewWorkspace {
                source_workspace_id,
                cwd,
                suggested_name,
            } => Some(shepr_api::schema::Method::WorkspaceCreate(
                shepr_api::schema::WorkspaceCreateParams {
                    source_workspace_id,
                    cwd,
                    focus: true,
                    label: (!trimmed.is_empty() && trimmed != suggested_name)
                        .then(|| trimmed.to_owned()),
                    env: Default::default(),
                },
            )),
            ClientRenameTarget::Workspace { workspace_id } => (!trimmed.is_empty()).then(|| {
                shepr_api::schema::Method::WorkspaceRename(
                    shepr_api::schema::WorkspaceRenameParams {
                        workspace_id,
                        label: trimmed.to_owned(),
                    },
                )
            }),
            ClientRenameTarget::NewTab {
                workspace_id,
                default_name,
            } => Some(shepr_api::schema::Method::TabCreate(
                shepr_api::schema::TabCreateParams {
                    workspace_id: Some(workspace_id),
                    cwd: None,
                    focus: true,
                    label: (!trimmed.is_empty() && trimmed != default_name)
                        .then(|| trimmed.to_owned()),
                    env: Default::default(),
                },
            )),
            ClientRenameTarget::Tab {
                tab_id,
                auto_name,
                original_name,
            } => (!(trimmed.is_empty() || auto_name && trimmed == original_name)).then(|| {
                shepr_api::schema::Method::TabRename(shepr_api::schema::TabRenameParams {
                    tab_id: tab_id.to_string(),
                    label: trimmed.to_owned(),
                })
            }),
            ClientRenameTarget::Pane { pane_id } => Some(shepr_api::schema::Method::PaneRename(
                shepr_api::schema::PaneRenameParams {
                    pane_id: pane_id.to_string(),
                    label: Some(trimmed.to_owned()),
                },
            )),
        };
        if let Some(method) = method {
            self.push_endpoint_method(method, outcome);
        }
        outcome.repaint = true;
    }

    pub(super) fn request_tab_close(
        &mut self,
        tab_id: &shepr_protocol::PublicTabId,
        outcome: &mut ClientShellInput,
    ) {
        let workspace_id = self.snapshot.as_deref().and_then(|snapshot| {
            let target = snapshot.tabs.iter().find(|tab| &tab.tab_id == tab_id)?;
            (self.config.confirm_close
                && !snapshot
                    .tabs
                    .iter()
                    .any(|tab| tab.workspace_id == target.workspace_id && &tab.tab_id != tab_id))
            .then(|| target.workspace_id.to_string())
        });
        if let Some(workspace_id) = workspace_id
            && self.open_close_confirmation(workspace_id, Some(tab_id.clone()))
        {
            outcome.repaint = true;
            return;
        }
        self.push_endpoint_method(
            shepr_api::schema::Method::TabClose(shepr_api::schema::TabTarget {
                tab_id: tab_id.to_string(),
            }),
            outcome,
        );
    }

    pub(super) fn accept_close_confirmation(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::ConfirmClose(confirm)) = self.overlay.take() else {
            return;
        };
        outcome.repaint = true;
        let method = if let Some(target) = confirm.tab_target {
            if target.workspace.endpoint_id != self.active_endpoint_id
                || !self.navigation_target_valid(&target.workspace)
                || !self.snapshot.as_deref().is_some_and(|snapshot| {
                    snapshot.tabs.iter().any(|tab| {
                        tab.tab_id == target.tab_id
                            && tab.workspace_id == target.workspace.workspace_id
                    })
                })
            {
                self.receive_endpoint_unavailable(
                    "Close target changed; try closing the tab again".into(),
                );
                return;
            }
            shepr_api::schema::Method::TabClose(shepr_api::schema::TabTarget {
                tab_id: target.tab_id.to_string(),
            })
        } else {
            shepr_api::schema::Method::WorkspaceClose(shepr_api::schema::WorkspaceCloseParams {
                workspace_id: confirm.workspace_id,
            })
        };
        self.push_endpoint_method(method, outcome);
    }

    pub(super) fn open_confirm_close_overlay(&mut self, workspace_id: String) {
        self.open_close_confirmation(workspace_id, None);
    }

    fn open_close_confirmation(
        &mut self,
        workspace_id: String,
        tab_id: Option<shepr_protocol::PublicTabId>,
    ) -> bool {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return false;
        };
        let Some(workspace) = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == workspace_id)
        else {
            return false;
        };
        let tab_target = if let Some(tab_id) = tab_id {
            let Some(workspace) = self.navigation_target(&self.active_endpoint_id, &workspace_id)
            else {
                return false;
            };
            Some(ClientTabCloseConfirmation { tab_id, workspace })
        } else {
            None
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
                tab_target,
                title: "Close workspace?".to_owned(),
                detail: format!("{} \u{2014} {scope}", workspace.label),
                return_to_navigate: self.mode == ClientShellMode::Navigate,
            },
        ));
        true
    }
}
