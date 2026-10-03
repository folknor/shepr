use shepr_protocol::command::PaneDirection;
use shepr_protocol::command::PaneSwapParams;
use shepr_protocol::command::SplitDirection;

use crate::endpoint::ClientEndpointId;
use crate::shell::endpoints::ClientEndpointFocusTarget;
use crate::shell::ledger::Work;
use crate::shell::overlays::notices::ClientEndpointNoticeKind;
use crate::shell::overlays::text_editor::TextEditor;
use crate::shell::state::{
    ClientHelpOverlay, ClientShellAction, ClientShellInput, ClientShellState,
};
use crate::shell::state::{ClientShellMode, ClientShellOverlay};

use shepr_protocol::command::EndpointCommand;

impl ClientShellState {
    pub(in crate::shell) fn record_binding(
        &mut self,
        binding: &shepr_termio::input::KeybindAction,
        outcome: &mut ClientShellInput,
    ) {
        match binding {
            shepr_termio::input::KeybindAction::Detach => {
                outcome.detach = true;
            }
            shepr_termio::input::KeybindAction::ToggleSidebar => {
                self.chrome.toggle_collapsed();
                self.reveal_navigation_workspace = true;
                // The retained surface stays on screen, clipped to the new pane area, until the
                // endpoint answers the resize.
                outcome.repaint = true;
                outcome.resize = true;
                self.persist_chrome_preferences(outcome);
            }
            action => {
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
                                    source: match self.workspace_action_id() {
                                        Some(workspace_id) => {
                                            shepr_protocol::command::WorkspaceCreateSource::Follow(
                                                workspace_id,
                                            )
                                        }
                                        None => {
                                            shepr_protocol::command::WorkspaceCreateSource::Default
                                        }
                                    },
                                    label: None,
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
                                    shepr_protocol::command::WorkspaceCloseParams { workspace_id },
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
    pub(in crate::shell) fn request_selection_copy(&mut self, outcome: &mut ClientShellInput) {
        let Some(selection) = self.mouse_selection.selection.as_ref() else {
            return;
        };
        let pane_id = selection.pane_id.clone();
        let (anchor, cursor) = match selection.shape() {
            shepr_vt::selection::SelectionShape::Range => selection.ordered_cells(),
            shepr_vt::selection::SelectionShape::Lines => {
                let (start, end) = selection.ordered_rows();
                let width = self
                    .hits
                    .panes
                    .iter()
                    .find(|hit| hit.pane_id == pane_id)
                    .map(|hit| hit.inner_rect.width)
                    .or_else(|| {
                        self.copy_mode
                            .as_ref()
                            .filter(|copy_mode| copy_mode.pane_id == pane_id)
                            .map(|copy_mode| copy_mode.geometry.0)
                    })
                    .filter(|width| *width > 0);
                let Some(width) = width else {
                    return;
                };
                ((start.row, 0), (end.row, width.saturating_sub(1)))
            }
        };
        self.submit(
            EndpointCommand::PaneSelectionRead(shepr_protocol::command::PaneSelectionReadParams {
                pane_id,
                anchor: shepr_protocol::command::PaneTextPoint {
                    row: anchor.0,
                    col: anchor.1,
                },
                cursor: shepr_protocol::command::PaneTextPoint {
                    row: cursor.0,
                    col: cursor.1,
                },
            }),
            Work::SelectionCopy,
            outcome,
        );
    }

    pub(in crate::shell) fn push_endpoint_notice(
        &mut self,
        kind: ClientEndpointNoticeKind,
        code: impl Into<String>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> bool {
        let boot_id = if kind == ClientEndpointNoticeKind::Unavailable {
            None
        } else {
            self.snapshot
                .as_deref()
                .map(|snapshot| snapshot.boot_id.clone())
        };
        self.push_endpoint_notice_at_boot(boot_id, kind, code, title, body)
    }

    pub(in crate::shell) fn push_endpoint_notice_at_boot(
        &mut self,
        boot_id: Option<shepr_protocol::BootId>,
        kind: ClientEndpointNoticeKind,
        code: impl Into<String>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> bool {
        self.notices.push(boot_id, kind, code, title, body)
    }

    pub(crate) fn receive_paste_rejection(&mut self, message: String) -> bool {
        self.push_endpoint_notice(
            ClientEndpointNoticeKind::Rejected,
            "paste_rejected",
            "Paste rejected",
            message,
        )
    }

    /// Snapshot metadata is accepted from every endpoint, independent of surface ownership.
    /// Keep each boot's card until it has had its own drawn lifetime, even across switches.
    pub(crate) fn receive_restore_notice(
        &mut self,
        endpoint_id: &ClientEndpointId,
        boot_id: &shepr_protocol::BootId,
        notice: &shepr_protocol::SessionRestoreNotice,
    ) -> bool {
        self.queue_boot_notice(
            endpoint_id,
            boot_id,
            "session_restore_incomplete",
            "saved session not fully restored",
            notice.to_string(),
        )
    }

    /// The endpoint's server stopped saving its session for the rest of this
    /// boot. Shown once per boot like the restore card.
    pub(crate) fn receive_session_saves_stopped(
        &mut self,
        endpoint_id: &ClientEndpointId,
        boot_id: &shepr_protocol::BootId,
    ) -> bool {
        self.queue_boot_notice(
            endpoint_id,
            boot_id,
            "session_saves_stopped",
            "session saves stopped",
            "The server stopped saving its session after an internal failure (see the server log). \
             Layout changes from now on are not restored when the server next starts."
                .to_owned(),
        )
    }

    /// Queues a card an endpoint's snapshot carries for its whole boot, once
    /// per boot and `code`.
    fn queue_boot_notice(
        &mut self,
        endpoint_id: &ClientEndpointId,
        boot_id: &shepr_protocol::BootId,
        code: &str,
        title: &str,
        body: String,
    ) -> bool {
        self.notices
            .queue_boot(endpoint_id, boot_id, code, title, body)
    }

    /// Shows a notice an endpoint's server sent.
    pub(crate) fn receive_server_notice(&mut self, kind: &shepr_protocol::NoticeKind) -> bool {
        let (code, title) = match kind {
            shepr_protocol::NoticeKind::PaneInputDropped { .. } => (
                "pane_input_dropped".to_owned(),
                "Pane input dropped".to_owned(),
            ),
            shepr_protocol::NoticeKind::PasteRejected { .. } => {
                ("paste_rejected".to_owned(), "Paste rejected".to_owned())
            }
            shepr_protocol::NoticeKind::OversizedSurface { .. } => (
                "oversized_surface".to_owned(),
                "Screen too large".to_owned(),
            ),
        };
        self.push_endpoint_notice(
            ClientEndpointNoticeKind::Rejected,
            code,
            title,
            kind.to_string(),
        )
    }

    pub(crate) fn receive_endpoint_unavailable(&mut self, message: String) -> bool {
        self.push_endpoint_notice_at_boot(
            None,
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
        let workspace_id = match &target {
            ClientEndpointFocusTarget::Workspace(workspace_id) => Some(workspace_id.clone()),
            ClientEndpointFocusTarget::Pane(_) => None,
        };
        let command = match target {
            ClientEndpointFocusTarget::Workspace(workspace_id) => {
                EndpointCommand::WorkspaceFocus(shepr_protocol::command::WorkspaceTarget {
                    workspace_id,
                })
            }
            ClientEndpointFocusTarget::Pane(pane_id) => {
                EndpointCommand::PaneFocus(shepr_protocol::command::PaneTarget { pane_id })
            }
        };
        let mut outcome = ClientShellInput::default();
        let request = self.submit(command, Work::Plain, &mut outcome);
        if let (Some(workspace_id), Some(request)) = (workspace_id, request)
            && let Some(target) = self.navigation_target(&self.active_endpoint_id, &workspace_id)
        {
            self.keep_workspace_highlight_until_snapshot(target, &request, self.now);
        }
        outcome.actions
    }

    pub(in crate::shell) fn endpoint_command_for_action(
        &mut self,
        action: shepr_termio::input::KeybindAction,
    ) -> Option<EndpointCommand> {
        use shepr_protocol::command::{
            PaneFocusDirectionParams, PaneResizeParams, PaneSplitParams, PaneTarget,
            PaneZoomParams, WorkspaceTarget,
        };
        use shepr_termio::input::KeybindAction;

        let snapshot = self.snapshot.as_deref()?;
        let focused_workspace = snapshot.focused_workspace_id.as_ref();
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
            KeybindAction::FocusAgent(_)
            | KeybindAction::PreviousAgent
            | KeybindAction::NextAgent => {
                let agents = self.agent_panel_model.targets();
                let index = crate::shell::navigation::aggregate_navigation::agent_target_index(
                    agents,
                    &self.active_endpoint_id,
                    snapshot.focused_pane_id.as_deref(),
                    action,
                )?;
                let target = agents.get(index)?;
                if target.endpoint_id != self.active_endpoint_id {
                    return None;
                }
                let target_endpoint_id = target.endpoint_id.clone();
                let pane_id = target.pane_id.clone();
                // Relative moves can land on a row scrolled out of the sidebar;
                // bring it into view, as a numbered pick already names a shown one.
                if matches!(
                    action,
                    KeybindAction::PreviousAgent | KeybindAction::NextAgent
                ) && !self
                    .hits
                    .agents
                    .iter()
                    .any(|(_, visible_pane_id)| *visible_pane_id == pane_id)
                {
                    let body_height = self.hits.agent_body.height;
                    self.reveal_endpoint_agent(&target_endpoint_id, &pane_id, body_height);
                }
                Some(EndpointCommand::PaneFocus(PaneTarget { pane_id }))
            }
            KeybindAction::SwitchWorkspace(index) => {
                let workspace_id = snapshot.workspaces.get(index)?.workspace_id.clone();
                self.reveal_workspace(&workspace_id);
                Some(EndpointCommand::WorkspaceFocus(WorkspaceTarget {
                    workspace_id,
                }))
            }
            KeybindAction::PreviousWorkspace | KeybindAction::NextWorkspace => {
                let workspaces = &snapshot.workspaces;
                if workspaces.is_empty() {
                    return None;
                }
                let current = workspaces
                    .iter()
                    .position(|workspace| Some(&workspace.workspace_id) == focused_workspace);
                let delta = if action == KeybindAction::PreviousWorkspace {
                    -1
                } else {
                    1
                };
                let next = crate::shell::navigation::aggregate_navigation::cycle_index(
                    workspaces.len(),
                    current,
                    delta,
                )?;
                let workspace_id = workspaces[next].workspace_id.clone();
                self.reveal_workspace(&workspace_id);
                Some(EndpointCommand::WorkspaceFocus(WorkspaceTarget {
                    workspace_id,
                }))
            }
            KeybindAction::FocusPaneLeft
            | KeybindAction::FocusPaneDown
            | KeybindAction::FocusPaneUp
            | KeybindAction::FocusPaneRight => Some(EndpointCommand::PaneFocusDirection(
                PaneFocusDirectionParams {
                    pane_id: focused_pane.clone()?,
                    direction: direction(action)?,
                },
            )),
            KeybindAction::SwapPaneLeft
            | KeybindAction::SwapPaneDown
            | KeybindAction::SwapPaneUp
            | KeybindAction::SwapPaneRight => {
                Some(EndpointCommand::PaneSwap(PaneSwapParams::Direction {
                    pane_id: focused_pane.clone()?,
                    direction: direction(action)?,
                }))
            }
            KeybindAction::SplitVertical | KeybindAction::SplitHorizontal => {
                Some(EndpointCommand::PaneSplit(PaneSplitParams {
                    pane_id: focused_pane.clone()?,
                    direction: if action == KeybindAction::SplitVertical {
                        SplitDirection::Right
                    } else {
                        SplitDirection::Down
                    },
                }))
            }
            KeybindAction::ClosePane => Some(EndpointCommand::PaneClose(PaneTarget {
                pane_id: focused_pane.clone()?,
            })),
            KeybindAction::CyclePaneNext | KeybindAction::CyclePanePrevious => {
                let panes = snapshot
                    .panes
                    .iter()
                    .filter(|pane| {
                        focused_workspace
                            .is_some_and(|workspace_id| &pane.workspace_id == workspace_id)
                    })
                    .collect::<Vec<_>>();
                if panes.is_empty() {
                    return None;
                }
                let focused_pane = focused_pane?;
                let current = panes.iter().position(|pane| pane.pane_id == focused_pane);
                let delta = if action == KeybindAction::CyclePanePrevious {
                    -1
                } else {
                    1
                };
                let next = crate::shell::navigation::aggregate_navigation::cycle_index(
                    panes.len(),
                    current,
                    delta,
                )?;
                Some(EndpointCommand::PaneFocus(PaneTarget {
                    pane_id: panes[next].pane_id.clone(),
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
                    pane_id: pane_id.clone(),
                }))
            }
            KeybindAction::Zoom => Some(EndpointCommand::PaneZoom(PaneZoomParams {
                pane_id: focused_pane?,
            })),
            KeybindAction::ClearPane => Some(EndpointCommand::PaneClear(PaneTarget {
                pane_id: focused_pane?,
            })),
            KeybindAction::ResizePaneLeft
            | KeybindAction::ResizePaneDown
            | KeybindAction::ResizePaneUp
            | KeybindAction::ResizePaneRight => {
                Some(EndpointCommand::PaneResize(PaneResizeParams {
                    pane_id: focused_pane?,
                    direction: direction(action)?,
                }))
            }
            _ => None,
        }
    }
}
