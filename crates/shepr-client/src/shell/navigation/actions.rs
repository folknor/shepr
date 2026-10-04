use shepr_protocol::command::PaneDirection;
use shepr_protocol::command::PaneSwapParams;
use shepr_protocol::command::SplitDirection;

use crate::endpoint::ClientEndpointId;
use crate::shell::EndpointNotice;
use crate::shell::ledger::{Submitted, Work};
use crate::shell::navigation::location::{Location, LocationTarget};
use crate::shell::notices::{BootNoticeCode, ClientEndpointNoticeKind, NoticeCode};
use crate::shell::overlays::Overlay;
use crate::shell::overlays::help::HelpOverlay;
use crate::shell::state::ClientShellMode;
use crate::shell::state::{ClientShellAction, ClientShellInput, ClientShellState};

use shepr_protocol::command::EndpointCommand;

/// A command for the presented endpoint and the sidebar reveal it implies.
pub(in crate::shell) struct ActionCommand {
    pub(in crate::shell) command: EndpointCommand,
    reveal: Option<ActionReveal>,
}

enum ActionReveal {
    Workspace(shepr_protocol::WorkspaceId),
    Agent(Location),
}

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
                self.sidebar_scroll.reveal_selected_workspace();
                // The retained surface stays on screen, clipped to the new pane area, until the
                // endpoint answers the resize.
                outcome.repaint = true;
                outcome.resize = true;
                self.persist_chrome_preferences(outcome);
            }
            action => {
                let action = *action;
                if action == shepr_termio::input::KeybindAction::OpenNavigator {
                    self.open_navigator_overlay();
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::Help {
                    self.overlay = Some(Overlay::Help(HelpOverlay::default()));
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::NewWorkspace {
                    if self.config.prompt_new_workspace_name {
                        self.open_new_workspace_overlay();
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
                    let preview = self.focused_navigation_target();
                    self.mode.enter_navigate(preview);
                    self.sidebar_scroll.reveal_selected_workspace();
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::EnterResizeMode {
                    self.mode.set(ClientShellMode::Resize);
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
                if let Some(ActionCommand { command, reveal }) =
                    self.endpoint_command_for_action(action)
                {
                    self.push_endpoint_command(command, outcome);
                    match reveal {
                        Some(ActionReveal::Workspace(workspace_id)) => {
                            self.request_workspace_reveal(&workspace_id);
                        }
                        Some(ActionReveal::Agent(location)) => {
                            self.sidebar_scroll.reveal_agent(location);
                        }
                        None => {}
                    }
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
        let pane_id = *selection.pane_id();
        let (anchor, cursor) = match selection.shape() {
            shepr_term::selection::SelectionShape::Range => selection.ordered_rows(),
            shepr_term::selection::SelectionShape::Lines => {
                let (start, end) = selection.ordered_rows();
                let width = self
                    .presentation
                    .pane_hits()
                    .iter()
                    .find(|hit| hit.pane_id == pane_id)
                    .map(|hit| hit.inner_rect.width)
                    .or_else(|| {
                        self.copy
                            .as_ref()
                            .filter(|copy_mode| copy_mode.pane_id == pane_id)
                            .map(|copy_mode| copy_mode.geometry.0)
                    })
                    .filter(|width| *width > 0);
                let Some(width) = width else {
                    return;
                };
                (
                    shepr_term::Point::new(start.row, 0),
                    shepr_term::Point::new(end.row, width.saturating_sub(1)),
                )
            }
        };
        self.submit(
            EndpointCommand::PaneSelectionRead(shepr_protocol::command::PaneSelectionReadParams {
                pane_id,
                anchor: shepr_protocol::command::PaneTextPoint {
                    row: anchor.row,
                    col: anchor.col,
                },
                cursor: shepr_protocol::command::PaneTextPoint {
                    row: cursor.row,
                    col: cursor.col,
                },
            }),
            Work::SelectionCopy,
            outcome,
        );
    }

    pub(in crate::shell) fn push_endpoint_notice(
        &mut self,
        kind: ClientEndpointNoticeKind,
        code: NoticeCode,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> bool {
        let boot_id = if kind == ClientEndpointNoticeKind::Unavailable {
            None
        } else {
            self.endpoints
                .active
                .snapshot()
                .map(|snapshot| snapshot.boot_id.clone())
        };
        self.push_endpoint_notice_at_boot(boot_id, kind, code, title, body)
    }

    pub(in crate::shell) fn push_endpoint_notice_at_boot(
        &mut self,
        boot_id: Option<shepr_protocol::BootId>,
        kind: ClientEndpointNoticeKind,
        code: NoticeCode,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> bool {
        self.notices.push(boot_id, kind, code, title, body)
    }

    pub(in crate::shell) fn receive_paste_rejection(&mut self, message: String) -> bool {
        self.push_endpoint_notice(
            ClientEndpointNoticeKind::Rejected,
            NoticeCode::PasteRejected,
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
            BootNoticeCode::SessionRestoreIncomplete,
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
            BootNoticeCode::SessionSavesStopped,
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
        code: BootNoticeCode,
        title: &str,
        body: String,
    ) -> bool {
        let label = endpoint_id.display_label(&self.config.local_label);
        self.notices
            .queue_boot(endpoint_id, label, boot_id, code, title, body)
    }

    /// Shows a notice an endpoint's server sent.
    pub(crate) fn receive_server_notice(&mut self, kind: &shepr_protocol::NoticeKind) -> bool {
        let (code, title) = match kind {
            shepr_protocol::NoticeKind::PaneInputDropped { .. } => (
                NoticeCode::PaneInputDropped,
                "Pane input dropped".to_owned(),
            ),
            shepr_protocol::NoticeKind::LimitExceeded(error) => match error.limit.kind() {
                shepr_protocol::LimitKind::InputPayloadBytes => {
                    (NoticeCode::PasteRejected, "Paste rejected".to_owned())
                }
                shepr_protocol::LimitKind::SurfaceMessageBytes => {
                    (NoticeCode::OversizedSurface, "Screen too large".to_owned())
                }
                _ => (NoticeCode::SizeLimit, "Size limit reached".to_owned()),
            },
        };
        self.push_endpoint_notice(
            ClientEndpointNoticeKind::Rejected,
            code,
            title,
            kind.to_string(),
        )
    }

    pub(crate) fn receive_endpoint_unavailable(&mut self, notice: &EndpointNotice) -> bool {
        self.push_endpoint_notice_at_boot(
            None,
            ClientEndpointNoticeKind::Unavailable,
            NoticeCode::EndpointUnavailable,
            "Endpoint unavailable",
            notice.body(&self.config.local_label),
        )
    }

    pub(crate) fn focus_endpoint_target(
        &mut self,
        target: LocationTarget,
    ) -> Vec<ClientShellAction> {
        let workspace_id = match &target {
            LocationTarget::Workspace(workspace_id) => Some(*workspace_id),
            LocationTarget::Pane(_) | LocationTarget::Machine => None,
        };
        let command = match target {
            LocationTarget::Workspace(workspace_id) => {
                EndpointCommand::WorkspaceFocus(shepr_protocol::command::WorkspaceTarget {
                    workspace_id,
                })
            }
            LocationTarget::Pane(pane_id) => {
                EndpointCommand::PaneFocus(shepr_protocol::command::PaneTarget { pane_id })
            }
            // The endpoint itself has no navigation to carry out.
            LocationTarget::Machine => return Vec::new(),
        };
        let mut outcome = ClientShellInput::default();
        let highlight = self.ledger.ticket();
        let submitted = self.submit(command, Work::Focus { highlight }, &mut outcome);
        if let (Some(workspace_id), Submitted::Opened) = (workspace_id, submitted)
            && let Some(target) = self.navigation_target(self.endpoints.presented(), &workspace_id)
        {
            self.keep_workspace_highlight_until_snapshot(target, highlight, self.now);
        }
        outcome.actions
    }

    /// The command a keybinding sends the presented endpoint, with the sidebar reveal it
    /// implies. The caller requests the reveal; nothing here touches scroll state.
    pub(in crate::shell) fn endpoint_command_for_action(
        &self,
        action: shepr_termio::input::KeybindAction,
    ) -> Option<ActionCommand> {
        use shepr_termio::input::KeybindAction;

        let command = self.plain_command_for_action(action)?;
        let reveal = match (&command, action) {
            (
                EndpointCommand::WorkspaceFocus(target),
                KeybindAction::SwitchWorkspace(_)
                | KeybindAction::PreviousWorkspace
                | KeybindAction::NextWorkspace,
            ) => Some(ActionReveal::Workspace(target.workspace_id)),
            // Relative moves can land on a row scrolled out of the sidebar, so they ask
            // for it to be brought into view (a no-op for a shown row); a numbered pick
            // already names a shown one.
            (
                EndpointCommand::PaneFocus(target),
                KeybindAction::PreviousAgent | KeybindAction::NextAgent,
            ) => Some(ActionReveal::Agent(Location::pane(
                self.endpoints.presented().clone(),
                target.pane_id,
            ))),
            _ => None,
        };
        Some(ActionCommand { command, reveal })
    }

    fn plain_command_for_action(
        &self,
        action: shepr_termio::input::KeybindAction,
    ) -> Option<EndpointCommand> {
        use shepr_protocol::command::{
            PaneFocusDirectionParams, PaneResizeParams, PaneSplitParams, PaneTarget,
            PaneZoomParams, WorkspaceTarget,
        };
        use shepr_termio::input::KeybindAction;

        let snapshot = self.endpoints.active.snapshot()?;
        let focused_workspace = snapshot.focused_workspace_id.as_ref();
        let focused_pane = snapshot.focused_pane_id;
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
                let agents = self.endpoints.agent_panel_model.targets();
                let index = crate::shell::navigation::aggregate_navigation::agent_target_index(
                    agents,
                    self.endpoints.presented(),
                    snapshot.focused_pane_id.as_ref(),
                    action,
                )?;
                let target = agents.get(index)?;
                if target.endpoint != *self.endpoints.presented() {
                    return None;
                }
                let pane_id = target.pane_id()?;
                Some(EndpointCommand::PaneFocus(PaneTarget { pane_id }))
            }
            KeybindAction::SwitchWorkspace(index) => {
                let workspace_id = snapshot.workspaces.get(index)?.workspace_id;
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
                let workspace_id = workspaces[next].workspace_id;
                Some(EndpointCommand::WorkspaceFocus(WorkspaceTarget {
                    workspace_id,
                }))
            }
            KeybindAction::FocusPaneLeft
            | KeybindAction::FocusPaneDown
            | KeybindAction::FocusPaneUp
            | KeybindAction::FocusPaneRight => Some(EndpointCommand::PaneFocusDirection(
                PaneFocusDirectionParams {
                    pane_id: focused_pane?,
                    direction: direction(action)?,
                },
            )),
            KeybindAction::SwapPaneLeft
            | KeybindAction::SwapPaneDown
            | KeybindAction::SwapPaneUp
            | KeybindAction::SwapPaneRight => {
                Some(EndpointCommand::PaneSwap(PaneSwapParams::Direction {
                    pane_id: focused_pane?,
                    direction: direction(action)?,
                }))
            }
            KeybindAction::SplitVertical | KeybindAction::SplitHorizontal => {
                Some(EndpointCommand::PaneSplit(PaneSplitParams {
                    pane_id: focused_pane?,
                    direction: if action == KeybindAction::SplitVertical {
                        SplitDirection::Right
                    } else {
                        SplitDirection::Down
                    },
                }))
            }
            KeybindAction::ClosePane => Some(EndpointCommand::PaneClose(PaneTarget {
                pane_id: focused_pane?,
            })),
            KeybindAction::CyclePaneNext | KeybindAction::CyclePanePrevious => {
                let panes = snapshot
                    .panes
                    .iter()
                    .filter(|pane| {
                        focused_workspace
                            .is_some_and(|workspace_id| pane.pane_id.workspace_id() == workspace_id)
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
                    pane_id: panes[next].pane_id,
                }))
            }
            KeybindAction::LastPane => {
                let pane_id = self.endpoints.active.previous_pane_id()?;
                if Some(pane_id) == focused_pane.as_ref()
                    || !snapshot.panes.iter().any(|pane| &pane.pane_id == pane_id)
                {
                    return None;
                }
                Some(EndpointCommand::PaneFocus(PaneTarget { pane_id: *pane_id }))
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
