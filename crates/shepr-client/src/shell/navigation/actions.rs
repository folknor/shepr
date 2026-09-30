use super::*;
use shepr_protocol::command::{EndpointCommand, EndpointReply};

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
                        self.open_new_workspace_overlay(outcome);
                    } else {
                        self.push_endpoint_command(
                            EndpointCommand::WorkspaceCreate(
                                shepr_protocol::command::WorkspaceCreateParams {
                                    source_workspace_id: self.workspace_action_id().map(Into::into),
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
                            self.push_endpoint_command(
                                EndpointCommand::WorkspaceClose(
                                    shepr_protocol::command::WorkspaceCloseParams {
                                        workspace_id: workspace_id.into(),
                                    },
                                ),
                                outcome,
                            );
                        }
                    }
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
                if let Some(command) = self.endpoint_command_for_action(action) {
                    self.push_endpoint_command(command, outcome);
                }
            }
        }
    }

    /// Copies the current selection. The read is by absolute row against the
    /// live terminal, with no content revision: output between the displayed
    /// frame and this request must not reject the copy.
    pub(super) fn request_selection_copy(&mut self, outcome: &mut ClientShellInput) {
        let Some(selection) = self.selection.as_ref() else {
            return;
        };
        let pane_id = selection.pane_id.clone();
        let (anchor, cursor) = selection.ordered_cells();
        self.push_endpoint_command_with_kind(
            EndpointCommand::PaneSelectionRead(shepr_protocol::command::PaneSelectionReadParams {
                pane_id: pane_id.to_string(),
                anchor: shepr_protocol::command::PaneTextPoint {
                    row: anchor.0,
                    col: anchor.1,
                },
                cursor: shepr_protocol::command::PaneTextPoint {
                    row: cursor.0,
                    col: cursor.1,
                },
            }),
            PendingEndpointKind::SelectionCopy,
            outcome,
        );
    }

    pub(super) fn push_endpoint_command(
        &mut self,
        command: EndpointCommand,
        outcome: &mut ClientShellInput,
    ) {
        self.push_endpoint_command_with_kind(command, PendingEndpointKind::Generic, outcome);
    }

    pub(super) fn push_endpoint_notice(
        &mut self,
        kind: ClientEndpointNoticeKind,
        code: impl Into<String>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> bool {
        let key = ClientEndpointNoticeKey {
            boot_id: self
                .snapshot
                .as_deref()
                .map(|snapshot| snapshot.boot_id.clone()),
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

    pub(super) fn push_endpoint_command_with_kind(
        &mut self,
        command: EndpointCommand,
        kind: PendingEndpointKind,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let changes_focus = match &command {
            EndpointCommand::WorkspaceFocus(_)
            | EndpointCommand::PaneFocus(_)
            | EndpointCommand::PaneFocusDirection(_) => true,
            EndpointCommand::WorkspaceCreate(params) => params.focus,
            EndpointCommand::PaneSplit(params) => params.focus,
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
        let method_name = command.name().to_owned();
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
            request: Box::new(ClientShellEndpointRequest {
                id: request_id.to_string(),
                command,
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
        let command = match target {
            ClientEndpointFocusTarget::Workspace(workspace_id) => {
                EndpointCommand::WorkspaceFocus(shepr_protocol::command::WorkspaceTarget {
                    workspace_id: workspace_id.into(),
                })
            }
            ClientEndpointFocusTarget::Pane(pane_id) => {
                EndpointCommand::PaneFocus(shepr_protocol::command::PaneTarget {
                    pane_id: pane_id.to_string(),
                })
            }
        };
        let mut outcome = ClientShellInput::default();
        self.push_endpoint_command(command, &mut outcome);
        outcome.actions
    }

    pub(crate) fn cancel_endpoint_request(&mut self, request_id: &str) -> bool {
        let Some(pending) = self.pending_requests.get(request_id) else {
            return false;
        };
        let boot_id = pending.boot_id.clone();
        // A cancellation is always an error result, and no error path schedules
        // a deadline, so the instant is never compared; it only satisfies the
        // shared result path.
        let outcome = self.handle_endpoint_result_at(
            &boot_id,
            request_id,
            Err(ClientShellEndpointError {
                code: crate::endpoint::commands::EndpointFailureCode::Cancelled,
                message: "This server action was interrupted. Check its state before retrying."
                    .into(),
            }),
            self.now,
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

    pub(crate) fn handle_endpoint_result_at(
        &mut self,
        boot_id: &str,
        request_id: &str,
        result: Result<EndpointReply, ClientShellEndpointError>,
        now: std::time::Instant,
    ) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        let (repaint, actions) =
            self.apply_endpoint_result(boot_id, request_id, result, &mut outcome, now);
        outcome.repaint |= repaint;
        outcome.actions.extend(actions);
        outcome
    }

    fn apply_endpoint_result(
        &mut self,
        boot_id: &str,
        request_id: &str,
        result: Result<EndpointReply, ClientShellEndpointError>,
        outcome: &mut ClientShellInput,
        now: std::time::Instant,
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
                boot_id: Some(pending.boot_id.clone()),
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
            let code = error.code.as_str();
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
        match pending.kind {
            PendingEndpointKind::Generic => {}
            PendingEndpointKind::WorkspaceLabel { lookup_id } => {
                return (
                    self.complete_workspace_label_lookup(lookup_id, result),
                    Vec::new(),
                );
            }
            PendingEndpointKind::PaneScroll { pane_id, serial } => {
                let repaint = self.complete_pane_scroll(&pane_id, serial, result, now, outcome);
                return (repaint, Vec::new());
            }
            PendingEndpointKind::SelectionCopy => {
                return match result {
                    Ok(EndpointReply::PaneSelection { text, .. }) if !text.is_empty() => (
                        false,
                        vec![ClientShellAction::ClipboardWrite(text.into_bytes())],
                    ),
                    Ok(EndpointReply::PaneSelection { .. }) => {
                        let shown = self.push_endpoint_notice(
                            ClientEndpointNoticeKind::Rejected,
                            "selection_empty",
                            "Nothing copied",
                            "The selection contained no text.",
                        );
                        (shown, Vec::new())
                    }
                    Ok(_) => {
                        self.set_endpoint_error(
                            "endpoint returned an unexpected selection result",
                            now,
                        );
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
                    now,
                );
            }
            PendingEndpointKind::CopyMotion {
                pane_id,
                origin,
                session_generation,
            } => {
                let (repaint, continue_queue) = match result {
                    Ok(EndpointReply::PaneCopyMotion {
                        pane_id: returned_pane_id,
                        cursor,
                    }) if returned_pane_id == pane_id => (
                        self.apply_copy_motion_target(&pane_id, origin, cursor, outcome),
                        true,
                    ),
                    Ok(EndpointReply::PaneCopyMotion { .. }) => (false, false),
                    Ok(_) => {
                        self.set_endpoint_error(
                            "endpoint returned an unexpected copy-motion result",
                            now,
                        );
                        (true, false)
                    }
                    Err(_) => (true, false),
                };
                self.complete_copy_operation(session_generation, continue_queue, now, outcome);
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
                    Ok(EndpointReply::PaneCopySearch {
                        pane_id: returned_pane_id,
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
                    Ok(EndpointReply::PaneCopySearch { .. }) => {
                        self.cancel_deferred_copy_after_search(generation);
                        (false, false)
                    }
                    Ok(_) => {
                        self.cancel_deferred_copy_after_search(generation);
                        self.set_endpoint_error(
                            "endpoint returned an unexpected copy-search result",
                            now,
                        );
                        (true, false)
                    }
                    Err(_) => {
                        self.cancel_deferred_copy_after_search(generation);
                        (true, false)
                    }
                };
                self.complete_copy_operation(session_generation, continue_queue, now, outcome);
                return (repaint, Vec::new());
            }
        }
        // Close confirmation is client-owned (`open_confirm_close_overlay` runs before the
        // close is sent); endpoints close without asking back.
        (result.is_err(), Vec::new())
    }

    pub(super) fn endpoint_command_for_action(
        &mut self,
        action: shepr_termio::input::KeybindAction,
    ) -> Option<EndpointCommand> {
        use shepr_protocol::command::{
            PaneDirection, PaneFocusDirectionParams, PaneResizeParams, PaneSplitParams,
            PaneSwapParams, PaneTarget, PaneZoomMode, PaneZoomParams, SplitDirection,
            WorkspaceTarget,
        };
        use shepr_termio::input::KeybindAction;

        let snapshot = self.snapshot.as_deref()?;
        let focused_workspace = snapshot
            .focused_workspace_id
            .as_ref()
            .map(ToString::to_string)?;
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
                Some(EndpointCommand::PaneFocus(PaneTarget {
                    pane_id: agents.get(index)?.to_string(),
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
                let current = agents
                    .iter()
                    .position(|pane_id| Some(pane_id) == snapshot.focused_pane_id.as_ref());
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
                    .any(|(_, visible_pane_id)| *visible_pane_id == pane_id)
                {
                    self.agent_scroll = next.min(self.hits.agent_max_scroll);
                }
                Some(EndpointCommand::PaneFocus(PaneTarget {
                    pane_id: pane_id.to_string(),
                }))
            }
            KeybindAction::SwitchWorkspace(index) => {
                let entries = self.navigation_workspace_entries(snapshot);
                let workspace_id = snapshot
                    .workspaces
                    .get(*entries.get(index)?)?
                    .workspace_id
                    .clone();
                self.reveal_workspace(&workspace_id);
                Some(EndpointCommand::WorkspaceFocus(WorkspaceTarget {
                    workspace_id: workspace_id.into(),
                }))
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
                let workspace_id = snapshot.workspaces[entries[next]].workspace_id.clone();
                self.reveal_workspace(&workspace_id);
                Some(EndpointCommand::WorkspaceFocus(WorkspaceTarget {
                    workspace_id: workspace_id.into(),
                }))
            }
            KeybindAction::FocusPaneLeft
            | KeybindAction::FocusPaneDown
            | KeybindAction::FocusPaneUp
            | KeybindAction::FocusPaneRight => Some(EndpointCommand::PaneFocusDirection(
                PaneFocusDirectionParams {
                    pane_id: focused_pane.clone().map(|id| id.to_string()),
                    direction: direction(action)?,
                },
            )),
            KeybindAction::SwapPaneLeft
            | KeybindAction::SwapPaneDown
            | KeybindAction::SwapPaneUp
            | KeybindAction::SwapPaneRight => Some(EndpointCommand::PaneSwap(PaneSwapParams {
                pane_id: focused_pane.clone().map(|id| id.to_string()),
                direction: Some(direction(action)?),
                source_pane_id: None,
                target_pane_id: None,
            })),
            KeybindAction::SplitVertical | KeybindAction::SplitHorizontal => {
                Some(EndpointCommand::PaneSplit(PaneSplitParams {
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
            KeybindAction::ClosePane => Some(EndpointCommand::PaneClose(PaneTarget {
                pane_id: focused_pane.clone()?.to_string(),
            })),
            KeybindAction::CyclePaneNext | KeybindAction::CyclePanePrevious => {
                let panes = snapshot
                    .panes
                    .iter()
                    .filter(|pane| pane.workspace_id == focused_workspace)
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
                Some(EndpointCommand::PaneFocus(PaneTarget {
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
                Some(EndpointCommand::PaneFocus(PaneTarget {
                    pane_id: pane_id.to_string(),
                }))
            }
            KeybindAction::Zoom => Some(EndpointCommand::PaneZoom(PaneZoomParams {
                pane_id: focused_pane.clone().map(|id| id.to_string()),
                mode: PaneZoomMode::Toggle,
            })),
            KeybindAction::ClearPane => Some(EndpointCommand::PaneClear(PaneTarget {
                pane_id: focused_pane?.to_string(),
            })),
            KeybindAction::ResizePaneLeft
            | KeybindAction::ResizePaneDown
            | KeybindAction::ResizePaneUp
            | KeybindAction::ResizePaneRight => {
                Some(EndpointCommand::PaneResize(PaneResizeParams {
                    pane_id: focused_pane.map(|id| id.to_string()),
                    direction: direction(action)?,
                    amount: None,
                }))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
impl ClientShellState {
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
        result: Result<EndpointReply, ClientShellEndpointError>,
    ) -> ClientShellInput {
        // clock-io-ok: this test-only wrapper stands in for the client loop.
        self.handle_endpoint_result_at(boot_id, request_id, result, std::time::Instant::now())
    }
}
