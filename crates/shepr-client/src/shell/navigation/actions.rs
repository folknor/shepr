use super::*;

impl ClientShellState {
    pub(super) fn record_binding(
        &mut self,
        binding: &shepr_termio::input::KeybindMatch,
        outcome: &mut ClientShellInput,
    ) {
        match binding {
            shepr_termio::input::KeybindMatch::Action(
                shepr_termio::input::KeybindAction::Detach,
            ) => {
                outcome.detach = true;
            }
            shepr_termio::input::KeybindMatch::Action(
                shepr_termio::input::KeybindAction::ToggleSidebar,
            ) => {
                self.sidebar_collapsed = !self.sidebar_collapsed;
                self.sidebar_collapsed_manual = true;
                self.reveal_navigation_workspace = true;
                // The retained surface stays on screen, clipped to the new pane area, until the
                // endpoint answers the resize.
                outcome.repaint = true;
                outcome.resize = true;
                self.persist_chrome_preferences(outcome);
            }
            shepr_termio::input::KeybindMatch::Action(action) => {
                let action = *action;
                if self.workspace_preview_action_blocked()
                    && matches!(
                        action,
                        shepr_termio::input::KeybindAction::RenameWorkspace
                            | shepr_termio::input::KeybindAction::CloseWorkspace
                    )
                {
                    let open_workspace = self.open_workspace_hint();
                    self.receive_endpoint_unavailable(format!(
                        "Select an available workspace and {open_workspace} before renaming or closing it"
                    ));
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::OpenNavigator {
                    self.open_navigator_overlay();
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::Help {
                    self.overlay = Some(ClientShellOverlay::Help(ClientHelpOverlay {
                        query: TextEditor::default(),
                        search_focused: false,
                        scroll: 0,
                    }));
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::NewWorkspace {
                    if self.config.prompt_new_workspace_name {
                        self.open_new_workspace_overlay();
                    } else {
                        self.push_endpoint_method(
                            shepr_api::schema::Method::WorkspaceCreate(
                                shepr_api::schema::WorkspaceCreateParams {
                                    source_workspace_id: self.workspace_action_id(),
                                    cwd: None,
                                    focus: true,
                                    label: None,
                                    env: Default::default(),
                                },
                            ),
                            outcome,
                        );
                    }
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::RenameWorkspace {
                    self.open_rename_workspace_overlay();
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::CloseWorkspace {
                    if let Some(workspace_id) = self.workspace_action_id() {
                        if self.config.confirm_close {
                            self.open_confirm_close_overlay(workspace_id);
                        } else {
                            self.push_endpoint_method(
                                shepr_api::schema::Method::WorkspaceClose(
                                    shepr_api::schema::WorkspaceCloseParams { workspace_id },
                                ),
                                outcome,
                            );
                        }
                    }
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::CloseTab {
                    if let Some(tab_id) = self
                        .snapshot
                        .as_deref()
                        .and_then(|snapshot| snapshot.focused_tab_id.clone())
                    {
                        self.request_tab_close(&tab_id, outcome);
                    }
                    return;
                }
                if action == shepr_termio::input::KeybindAction::NewTab
                    && self.config.prompt_new_tab_name
                {
                    self.open_new_tab_overlay();
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::RenameTab {
                    self.open_rename_tab_overlay();
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::RenamePane {
                    self.open_rename_pane_overlay();
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::WorkspacePicker {
                    self.pending_workspace_highlight = None;
                    self.mode = ClientShellMode::Navigate;
                    self.navigate_workspace_id = self.focused_navigation_target();
                    self.reveal_navigation_workspace = true;
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::EnterResizeMode {
                    self.mode = ClientShellMode::Resize;
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::CopyMode {
                    if self.enter_copy_mode(outcome) {
                        outcome.repaint = true;
                    }
                    return;
                }
                if self.handle_endpoint_navigation(action, outcome) {
                    return;
                }
                if let Some(method) = self.endpoint_method_for_action(action) {
                    self.push_endpoint_method(method, outcome);
                }
            }
        }
    }

    pub(super) fn request_selection_copy(&mut self, outcome: &mut ClientShellInput, live: bool) {
        let Some(selection) = self.selection.as_ref() else {
            return;
        };
        let pane_id = selection.pane_id.clone();
        let content_revision = self
            .pane_surface
            .as_ref()
            .and_then(|surface| surface.panes.iter().find(|pane| pane.pane_id == pane_id))
            .map(|pane| pane.content_revision)
            // Read an explicit selection atomically from the live terminal. Output
            // between the displayed frame and this request must not reject the copy.
            .filter(|_| !live);
        let (anchor, cursor) = selection.ordered_cells();
        self.push_endpoint_method_with_kind(
            shepr_api::schema::Method::PaneSelectionRead(
                shepr_api::schema::PaneSelectionReadParams {
                    pane_id: pane_id.to_string(),
                    anchor: shepr_api::schema::PaneSelectionPoint {
                        row: anchor.0,
                        col: anchor.1,
                    },
                    cursor: shepr_api::schema::PaneSelectionPoint {
                        row: cursor.0,
                        col: cursor.1,
                    },
                    content_revision,
                },
            ),
            PendingEndpointKind::SelectionCopy,
            outcome,
        );
    }

    pub(super) fn push_endpoint_method(
        &mut self,
        method: shepr_api::schema::Method,
        outcome: &mut ClientShellInput,
    ) {
        self.push_endpoint_method_with_kind(method, PendingEndpointKind::Generic, outcome);
    }

    pub(super) fn push_endpoint_notice(
        &mut self,
        kind: ClientEndpointNoticeKind,
        code: impl Into<String>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> bool {
        let key = ClientEndpointNoticeKey {
            boot_id: self.snapshot.as_deref().map_or_else(
                || "disconnected".into(),
                |snapshot| snapshot.boot_id.clone(),
            ),
            kind,
            code: code.into(),
        };
        let body = body.into();
        if kind == ClientEndpointNoticeKind::Rejected {
            if self
                .visible_endpoint_notice
                .as_ref()
                .is_some_and(|notice| notice.key == key && notice.body == body)
            {
                return false;
            }
        } else if !self.endpoint_notice_seen.insert(key.clone()) {
            return false;
        }
        self.visible_endpoint_notice = Some(ClientVisibleEndpointNotice {
            key,
            title: title.into(),
            body,
        });
        true
    }

    pub(super) fn push_endpoint_method_with_kind(
        &mut self,
        method: shepr_api::schema::Method,
        kind: PendingEndpointKind,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let changes_focus = match &method {
            shepr_api::schema::Method::WorkspaceFocus(_)
            | shepr_api::schema::Method::TabFocus(_)
            | shepr_api::schema::Method::PaneFocus(_)
            | shepr_api::schema::Method::PaneFocusDirection(_) => true,
            shepr_api::schema::Method::WorkspaceCreate(params) => params.focus,
            shepr_api::schema::Method::TabCreate(params) => params.focus,
            shepr_api::schema::Method::PaneSplit(params) => params.focus,
            _ => false,
        };
        if changes_focus {
            outcome.repaint |= self.pending_workspace_highlight.take().is_some();
        }
        if !self.endpoint_is_online(&self.active_endpoint_id) {
            let label = self.active_endpoint_label().to_owned();
            outcome.repaint |= self.receive_endpoint_unavailable(format!("{label} is not ready"));
            return false;
        }
        let method_name = shepr_api::api_method_name(&method).to_owned();
        let Some(snapshot) = self.snapshot.as_deref() else {
            return false;
        };
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        let request_id = shepr_protocol::RequestId::from(format!("client-shell:{request_id}"));
        self.pending_requests.insert(
            request_id.clone(),
            PendingEndpointRequest {
                boot_id: snapshot.boot_id.clone(),
                method_name,
                kind,
            },
        );
        outcome.actions.push(ClientShellAction::Endpoint {
            endpoint_id: self.active_endpoint_id.clone(),
            boot_id: snapshot.boot_id.clone(),
            request: Box::new(shepr_api::schema::Request {
                id: request_id.to_string(),
                method,
            }),
        });
        true
    }

    pub(crate) fn receive_endpoint_error(&mut self, message: String) -> bool {
        self.push_endpoint_notice(
            ClientEndpointNoticeKind::Rejected,
            "paste_rejected",
            "Paste rejected",
            message,
        )
    }

    pub(crate) fn receive_endpoint_unavailable(&mut self, message: String) -> bool {
        self.push_endpoint_notice(
            ClientEndpointNoticeKind::Unavailable,
            message.clone(),
            "Endpoint unavailable",
            message,
        )
    }

    pub(crate) fn focus_endpoint_target(
        &mut self,
        target: ClientEndpointFocusTarget,
    ) -> Vec<ClientShellAction> {
        let method = match target {
            ClientEndpointFocusTarget::Workspace(workspace_id) => {
                shepr_api::schema::Method::WorkspaceFocus(shepr_api::schema::WorkspaceTarget {
                    workspace_id,
                })
            }
            ClientEndpointFocusTarget::Pane(pane_id) => {
                shepr_api::schema::Method::PaneFocus(shepr_api::schema::PaneTarget {
                    pane_id: pane_id.to_string(),
                })
            }
        };
        let mut outcome = ClientShellInput::default();
        self.push_endpoint_method(method, &mut outcome);
        outcome.actions
    }

    pub(crate) fn cancel_endpoint_request(&mut self, request_id: &str) -> bool {
        let Some(pending) = self.pending_requests.get(request_id) else {
            return false;
        };
        let boot_id = pending.boot_id.clone();
        let outcome = self.handle_endpoint_result(
            &boot_id,
            request_id,
            Err(ClientShellEndpointError {
                code: Some(crate::endpoint::commands::EndpointFailureCode::Cancelled),
                message: "This server action was interrupted. Check its state before retrying."
                    .into(),
            }),
        );
        // A cancelled copy-mode request does not continue its key queue
        // (`continue_queue` is false on every error), so nothing but a repaint
        // can come out of it. Callers take only the repaint, so anything else
        // would be dropped; say so in the log instead of losing it silently.
        if !(outcome.actions.is_empty() && outcome.requests.is_empty()) {
            tracing::warn!(
                request_id,
                actions = outcome.actions.len(),
                requests = outcome.requests.len(),
                "cancelled endpoint request produced follow-up work, which is dropped"
            );
        }
        outcome.repaint
    }

    /// Applies an endpoint response and returns everything it produced.
    ///
    /// A copy-mode motion or search response replays the keys queued while it
    /// was in flight, and those keys can yield pane input, a resize, a detach or
    /// host queries, not just repaints and actions. The caller must route the
    /// whole outcome (`finish_client_shell_input`), or the replayed keystrokes
    /// are lost.
    pub(crate) fn handle_endpoint_result(
        &mut self,
        boot_id: &str,
        request_id: &str,
        result: Result<shepr_api::schema::ResponseResult, ClientShellEndpointError>,
    ) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        let (repaint, actions) =
            self.apply_endpoint_result(boot_id, request_id, result, &mut outcome);
        outcome.repaint |= repaint;
        outcome.actions.extend(actions);
        outcome
    }

    fn apply_endpoint_result(
        &mut self,
        boot_id: &str,
        request_id: &str,
        result: Result<shepr_api::schema::ResponseResult, ClientShellEndpointError>,
        outcome: &mut ClientShellInput,
    ) -> (bool, Vec<ClientShellAction>) {
        let Some(pending) = self.pending_requests.remove(request_id) else {
            return (false, Vec::new());
        };
        if pending.boot_id != boot_id
            || self
                .snapshot
                .as_deref()
                .is_none_or(|snapshot| snapshot.boot_id != boot_id)
        {
            return (false, Vec::new());
        }
        if result.is_ok() {
            let timeout_key = ClientEndpointNoticeKey {
                boot_id: boot_id.into(),
                kind: ClientEndpointNoticeKind::Timeout,
                code: pending.method_name.clone(),
            };
            self.endpoint_notice_seen.remove(&timeout_key);
        }
        if let Err(error) = &result {
            if self
                .pending_workspace_highlight
                .as_ref()
                .is_some_and(|pending| pending.request_id == request_id)
            {
                self.pending_workspace_highlight = None;
            }
            let code = error
                .code
                .as_ref()
                .map_or("invalid_response", |code| code.as_str());
            if !matches!(code, "stale_content" | "stale_target") {
                let (kind, notice_code, title, body) = match code {
                    "endpoint_timeout" => (
                        ClientEndpointNoticeKind::Timeout,
                        pending.method_name.clone(),
                        "Server timed out",
                        format!("This server did not respond to {}.", pending.method_name),
                    ),
                    "endpoint_cancelled" => (
                        ClientEndpointNoticeKind::Unavailable,
                        "cancelled".to_owned(),
                        "Action interrupted",
                        error.message.clone(),
                    ),
                    "server_unavailable" => (
                        ClientEndpointNoticeKind::Unavailable,
                        "server".to_owned(),
                        "Server unavailable",
                        error.message.clone(),
                    ),
                    _ => (
                        ClientEndpointNoticeKind::Rejected,
                        format!("{}:{code}", pending.method_name),
                        "Action rejected",
                        error.message.clone(),
                    ),
                };
                self.push_endpoint_notice(kind, notice_code, title, body);
            }
        }
        match pending.kind {
            PendingEndpointKind::Generic => {}
            PendingEndpointKind::PaneScroll { pane_id, serial } => {
                let repaint = self.complete_pane_scroll(&pane_id, serial, result, outcome);
                return (repaint, Vec::new());
            }
            PendingEndpointKind::SelectionCopy => {
                return match result {
                    Ok(shepr_api::schema::ResponseResult::PaneSelection { text, .. })
                        if !text.is_empty() =>
                    {
                        (
                            false,
                            vec![ClientShellAction::ClipboardWrite(text.into_bytes())],
                        )
                    }
                    Ok(shepr_api::schema::ResponseResult::PaneSelection { .. }) => {
                        (false, Vec::new())
                    }
                    Ok(_) => {
                        self.set_endpoint_error("endpoint returned an unexpected selection result");
                        (true, Vec::new())
                    }
                    Err(_) => (true, Vec::new()),
                };
            }
            PendingEndpointKind::WordSelection {
                pane_id,
                absolute_row,
                generation,
            } => {
                return self.complete_word_selection_row(
                    &pane_id,
                    absolute_row,
                    generation,
                    result,
                );
            }
            PendingEndpointKind::CopyMotion {
                pane_id,
                origin,
                session_generation,
            } => {
                let (repaint, continue_queue) = match result {
                    Ok(shepr_api::schema::ResponseResult::PaneCopyMotion {
                        pane_id: returned_pane_id,
                        cursor,
                        content_revision,
                    }) if returned_pane_id == pane_id => (
                        self.apply_copy_motion_target(
                            &pane_id,
                            origin,
                            cursor,
                            content_revision,
                            outcome,
                        ),
                        true,
                    ),
                    Ok(shepr_api::schema::ResponseResult::PaneCopyMotion { .. }) => (false, false),
                    Ok(_) => {
                        self.set_endpoint_error(
                            "endpoint returned an unexpected copy-motion result",
                        );
                        (true, false)
                    }
                    Err(_) => (true, false),
                };
                self.complete_copy_operation(session_generation, continue_queue, outcome);
                return (repaint, Vec::new());
            }
            PendingEndpointKind::CopySearch {
                pane_id,
                origin,
                query,
                direction,
                repeat,
                generation,
                session_generation,
            } => {
                let (repaint, continue_queue) = match result {
                    Ok(shepr_api::schema::ResponseResult::PaneCopySearch {
                        pane_id: returned_pane_id,
                        content_revision,
                        matches,
                        total,
                        current,
                        current_global,
                    }) if returned_pane_id == pane_id => {
                        let repaint = self.apply_copy_search_result(
                            &pane_id,
                            origin,
                            query,
                            direction,
                            repeat,
                            generation,
                            ClientCopySearchResult {
                                content_revision,
                                matches,
                                total,
                                current: current.and_then(|index| usize::try_from(index).ok()),
                                current_global,
                            },
                            outcome,
                        );
                        if !repaint {
                            self.cancel_deferred_copy_after_search(generation);
                        }
                        (repaint, repaint)
                    }
                    Ok(shepr_api::schema::ResponseResult::PaneCopySearch { .. }) => {
                        self.cancel_deferred_copy_after_search(generation);
                        (false, false)
                    }
                    Ok(_) => {
                        self.cancel_deferred_copy_after_search(generation);
                        self.set_endpoint_error(
                            "endpoint returned an unexpected copy-search result",
                        );
                        (true, false)
                    }
                    Err(_) => {
                        self.cancel_deferred_copy_after_search(generation);
                        (true, false)
                    }
                };
                self.complete_copy_operation(session_generation, continue_queue, outcome);
                return (repaint, Vec::new());
            }
        }
        // Close confirmation is client-owned (`open_confirm_close_overlay` runs before the
        // close is sent); endpoints close without asking back.
        (result.is_err(), Vec::new())
    }

    pub(super) fn endpoint_method_for_action(
        &mut self,
        action: shepr_termio::input::KeybindAction,
    ) -> Option<shepr_api::schema::Method> {
        use shepr_api::schema::{
            Method, PaneDirection, PaneFocusDirectionParams, PaneResizeParams, PaneSplitParams,
            PaneSwapParams, PaneTarget, PaneZoomMode, PaneZoomParams, SplitDirection,
            TabCreateParams, TabMoveParams, TabTarget, WorkspaceTarget,
        };
        use shepr_termio::input::KeybindAction;

        let snapshot = self.snapshot.as_deref()?;
        let focused_workspace = snapshot
            .focused_workspace_id
            .as_ref()
            .map(ToString::to_string)?;
        let focused_tab = snapshot.focused_tab_id.clone();
        let focused_pane = snapshot.focused_pane_id.clone();
        let direction = |action| match action {
            KeybindAction::FocusPaneLeft
            | KeybindAction::SwapPaneLeft
            | KeybindAction::ResizePaneLeft => Some(PaneDirection::Left),
            KeybindAction::FocusPaneDown
            | KeybindAction::SwapPaneDown
            | KeybindAction::ResizePaneDown => Some(PaneDirection::Down),
            KeybindAction::FocusPaneUp
            | KeybindAction::SwapPaneUp
            | KeybindAction::ResizePaneUp => Some(PaneDirection::Up),
            KeybindAction::FocusPaneRight
            | KeybindAction::SwapPaneRight
            | KeybindAction::ResizePaneRight => Some(PaneDirection::Right),
            _ => None,
        };

        match action {
            KeybindAction::FocusAgent(index) => {
                let agents = super::agent_sidebar::ordered_agent_pane_ids(
                    snapshot,
                    self.config.agent_panel_sort,
                );
                Some(Method::PaneFocus(PaneTarget {
                    pane_id: agents.get(index)?.clone(),
                }))
            }
            KeybindAction::PreviousAgent | KeybindAction::NextAgent => {
                let agents = super::agent_sidebar::ordered_agent_pane_ids(
                    snapshot,
                    self.config.agent_panel_sort,
                );
                if agents.is_empty() {
                    return None;
                }
                let current = agents.iter().position(|pane_id| {
                    Some(pane_id.as_str()) == snapshot.focused_pane_id.as_deref()
                });
                let next = match (current, action) {
                    (Some(current), KeybindAction::PreviousAgent) => {
                        (current + agents.len() - 1) % agents.len()
                    }
                    (Some(current), KeybindAction::NextAgent) => (current + 1) % agents.len(),
                    (None, KeybindAction::PreviousAgent) => agents.len() - 1,
                    (None, KeybindAction::NextAgent) => 0,
                    _ => unreachable!("relative agent action"),
                };
                let pane_id = agents[next].clone();
                if !self
                    .hits
                    .agents
                    .iter()
                    .any(|(_, visible_pane_id)| visible_pane_id.as_str() == pane_id)
                {
                    self.agent_scroll = next.min(self.hits.agent_max_scroll);
                }
                Some(Method::PaneFocus(PaneTarget { pane_id }))
            }
            KeybindAction::SwitchWorkspace(index) => {
                let entries = self.navigation_workspace_entries(snapshot);
                let workspace_id = snapshot
                    .workspaces
                    .get(*entries.get(index)?)?
                    .workspace_id
                    .to_string();
                self.reveal_workspace(&workspace_id);
                Some(Method::WorkspaceFocus(WorkspaceTarget { workspace_id }))
            }
            KeybindAction::PreviousWorkspace | KeybindAction::NextWorkspace => {
                let entries = self.navigation_workspace_entries(snapshot);
                if entries.is_empty() {
                    return None;
                }
                let current = entries
                    .iter()
                    .position(|entry| snapshot.workspaces[*entry].workspace_id == focused_workspace)
                    .unwrap_or(0);
                let delta = if action == KeybindAction::PreviousWorkspace {
                    -1
                } else {
                    1
                };
                let current_isize = isize::try_from(current).unwrap_or(isize::MAX);
                let len_isize = isize::try_from(entries.len()).unwrap_or(isize::MAX);
                let next = (current_isize + delta).rem_euclid(len_isize) as usize;
                let workspace_id = snapshot.workspaces[entries[next]].workspace_id.to_string();
                self.reveal_workspace(&workspace_id);
                Some(Method::WorkspaceFocus(WorkspaceTarget { workspace_id }))
            }
            KeybindAction::SwitchTab(index) => {
                let tabs = snapshot
                    .tabs
                    .iter()
                    .filter(|tab| tab.workspace_id == focused_workspace)
                    .collect::<Vec<_>>();
                Some(Method::TabFocus(TabTarget {
                    tab_id: tabs.get(index)?.tab_id.to_string(),
                }))
            }
            KeybindAction::PreviousTab | KeybindAction::NextTab => {
                let tabs = snapshot
                    .tabs
                    .iter()
                    .filter(|tab| tab.workspace_id == focused_workspace)
                    .collect::<Vec<_>>();
                let focused_tab = focused_tab?;
                let current = tabs.iter().position(|tab| tab.tab_id == focused_tab)?;
                let delta = if action == KeybindAction::PreviousTab {
                    -1
                } else {
                    1
                };
                let current_isize = isize::try_from(current).unwrap_or(isize::MAX);
                let len_isize = isize::try_from(tabs.len()).unwrap_or(isize::MAX);
                let next = (current_isize + delta).rem_euclid(len_isize) as usize;
                Some(Method::TabFocus(TabTarget {
                    tab_id: tabs[next].tab_id.to_string(),
                }))
            }
            KeybindAction::MoveTabPrevious | KeybindAction::MoveTabNext => {
                let tabs = snapshot
                    .tabs
                    .iter()
                    .filter(|tab| tab.workspace_id == focused_workspace)
                    .collect::<Vec<_>>();
                if tabs.len() <= 1 {
                    return None;
                }
                let focused_tab = focused_tab?;
                let source = tabs.iter().position(|tab| tab.tab_id == focused_tab)?;
                let insert_index = if action == KeybindAction::MoveTabNext {
                    if source + 1 >= tabs.len() {
                        0
                    } else {
                        source + 2
                    }
                } else if source == 0 {
                    tabs.len()
                } else {
                    source - 1
                };
                Some(Method::TabMove(TabMoveParams {
                    tab_id: focused_tab.to_string(),
                    insert_index,
                }))
            }
            KeybindAction::NewTab if !self.config.prompt_new_tab_name => {
                Some(Method::TabCreate(TabCreateParams {
                    workspace_id: Some(focused_workspace),
                    cwd: None,
                    focus: true,
                    label: None,
                    env: Default::default(),
                }))
            }
            KeybindAction::FocusPaneLeft
            | KeybindAction::FocusPaneDown
            | KeybindAction::FocusPaneUp
            | KeybindAction::FocusPaneRight => {
                Some(Method::PaneFocusDirection(PaneFocusDirectionParams {
                    pane_id: focused_pane.clone().map(|id| id.to_string()),
                    direction: direction(action)?,
                }))
            }
            KeybindAction::SwapPaneLeft
            | KeybindAction::SwapPaneDown
            | KeybindAction::SwapPaneUp
            | KeybindAction::SwapPaneRight => Some(Method::PaneSwap(PaneSwapParams {
                pane_id: focused_pane.clone().map(|id| id.to_string()),
                direction: Some(direction(action)?),
                source_pane_id: None,
                target_pane_id: None,
            })),
            KeybindAction::SplitVertical | KeybindAction::SplitHorizontal => {
                Some(Method::PaneSplit(PaneSplitParams {
                    workspace_id: Some(focused_workspace),
                    target_pane_id: focused_pane.clone().map(|id| id.to_string()),
                    direction: if action == KeybindAction::SplitVertical {
                        SplitDirection::Right
                    } else {
                        SplitDirection::Down
                    },
                    ratio: None,
                    cwd: None,
                    focus: true,
                    right_click: Default::default(),
                    env: Default::default(),
                }))
            }
            KeybindAction::ClosePane => Some(Method::PaneClose(PaneTarget {
                pane_id: focused_pane.clone()?.to_string(),
            })),
            KeybindAction::CyclePaneNext | KeybindAction::CyclePanePrevious => {
                let focused_tab = focused_tab?;
                let panes = snapshot
                    .panes
                    .iter()
                    .filter(|pane| pane.tab_id == focused_tab)
                    .collect::<Vec<_>>();
                if panes.is_empty() {
                    return None;
                }
                let focused_pane = focused_pane?;
                let current = panes
                    .iter()
                    .position(|pane| pane.pane_id == focused_pane)
                    .unwrap_or(0);
                let next = if action == KeybindAction::CyclePanePrevious {
                    (current + panes.len() - 1) % panes.len()
                } else {
                    (current + 1) % panes.len()
                };
                Some(Method::PaneFocus(PaneTarget {
                    pane_id: panes[next].pane_id.to_string(),
                }))
            }
            KeybindAction::LastPane => {
                let pane_id = self.previous_pane_id.as_ref()?;
                if Some(pane_id.as_str()) == focused_pane.as_deref()
                    || !snapshot.panes.iter().any(|pane| &pane.pane_id == pane_id)
                {
                    return None;
                }
                Some(Method::PaneFocus(PaneTarget {
                    pane_id: pane_id.to_string(),
                }))
            }
            KeybindAction::Zoom => Some(Method::PaneZoom(PaneZoomParams {
                pane_id: focused_pane.clone().map(|id| id.to_string()),
                mode: PaneZoomMode::Toggle,
            })),
            KeybindAction::ClearPane => Some(Method::PaneClear(PaneTarget {
                pane_id: focused_pane?.to_string(),
            })),
            KeybindAction::ResizePaneLeft
            | KeybindAction::ResizePaneDown
            | KeybindAction::ResizePaneUp
            | KeybindAction::ResizePaneRight => Some(Method::PaneResize(PaneResizeParams {
                pane_id: focused_pane.map(|id| id.to_string()),
                direction: direction(action)?,
                amount: None,
            })),
            _ => None,
        }
    }
}
