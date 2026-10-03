use shepr_protocol::command::PaneRightClickTarget;
use shepr_protocol::command::PaneSwapParams;
use shepr_protocol::command::SplitDirection;

use crate::shell::overlays::text_editor::TextEditor;
use crate::shell::state::{
    ClientContextMenuAction, ClientContextMenuItem, ClientContextMenuOverlay, ClientRenameOverlay,
    ClientShellInput, ClientShellState,
};
use crate::shell::state::{ClientContextMenuTarget, ClientRenameTarget, ClientShellOverlay};
use shepr_protocol::command::EndpointCommand;

impl ClientContextMenuOverlay {
    pub(in crate::shell) fn items(&self) -> Vec<ClientContextMenuItem> {
        use ClientContextMenuAction as Action;

        let item = |label, action| ClientContextMenuItem { label, action };
        match &self.target {
            ClientContextMenuTarget::Workspace { .. } => {
                vec![item("Rename", Action::Rename), item("Close", Action::Close)]
            }
            ClientContextMenuTarget::Pane {
                source_pane_id,
                has_manual_label,
                right_click_passthrough,
                ..
            } => {
                let mut items = vec![item("Rename pane", Action::RenamePane)];
                if *has_manual_label {
                    items.push(item("Clear pane name", Action::ClearPaneName));
                }
                if source_pane_id.is_some() {
                    items.push(item("Swap with focused pane", Action::SwapWithFocusedPane));
                }
                items.extend([
                    item("Split right", Action::SplitRight),
                    item("Split down", Action::SplitDown),
                    item("Zoom", Action::Zoom),
                    item(
                        if *right_click_passthrough {
                            "Use Shepr right-click menu"
                        } else {
                            "Send right-clicks to pane"
                        },
                        Action::ToggleRightClickPassthrough,
                    ),
                    item("Close pane", Action::ClosePane),
                ]);
                items
            }
        }
    }
}

impl ClientShellState {
    pub(in crate::shell) fn open_workspace_context_menu(
        &mut self,
        workspace_id: shepr_protocol::WorkspaceId,
        x: u16,
        y: u16,
    ) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        if !snapshot
            .workspaces
            .iter()
            .any(|workspace| workspace.workspace_id == workspace_id)
        {
            return;
        }
        self.overlay = Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            target: ClientContextMenuTarget::Workspace { workspace_id },
            x,
            y,
            highlighted: 0,
        }));
    }

    pub(in crate::shell) fn open_pane_context_menu(
        &mut self,
        pane_id: shepr_protocol::PublicPaneId,
        x: u16,
        y: u16,
    ) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let Some(pane) = snapshot.panes.iter().find(|pane| pane.pane_id == pane_id) else {
            return;
        };
        let source_pane_id = snapshot
            .focused_pane_id
            .filter(|focused| focused != &pane_id);
        self.overlay = Some(ClientShellOverlay::ContextMenu(ClientContextMenuOverlay {
            target: ClientContextMenuTarget::Pane {
                pane_id,
                source_pane_id,
                has_manual_label: pane.label.is_some(),
                right_click_passthrough: pane.right_click_passthrough,
            },
            x,
            y,
            highlighted: 0,
        }));
    }

    pub(in crate::shell) fn move_context_menu_selection(&mut self, delta: isize) {
        let Some(ClientShellOverlay::ContextMenu(menu)) = self.overlay.as_mut() else {
            return;
        };
        let item_count = menu.items().len();
        if item_count == 0 {
            return;
        }
        let max_index = item_count.saturating_sub(1);
        menu.highlighted = menu
            .highlighted
            .checked_add_signed(delta)
            .unwrap_or(0)
            .min(max_index);
    }

    pub(in crate::shell) fn activate_context_menu_item(
        &mut self,
        index: usize,
        outcome: &mut ClientShellInput,
    ) {
        let Some(ClientShellOverlay::ContextMenu(menu)) = self.overlay.take() else {
            return;
        };
        let Some(action) = menu.items().get(index).map(|item| item.action) else {
            outcome.repaint = true;
            return;
        };
        match menu.target {
            ClientContextMenuTarget::Workspace { workspace_id, .. } => {
                self.activate_workspace_context_action(workspace_id, action, outcome);
            }
            ClientContextMenuTarget::Pane {
                pane_id,
                source_pane_id,
                right_click_passthrough,
                ..
            } => self.activate_pane_context_action(
                pane_id,
                source_pane_id,
                right_click_passthrough,
                action,
                outcome,
            ),
        }
        outcome.repaint = true;
    }

    fn activate_workspace_context_action(
        &mut self,
        workspace_id: shepr_protocol::WorkspaceId,
        action: ClientContextMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        match action {
            ClientContextMenuAction::Rename => {
                let label = self
                    .snapshot
                    .as_deref()
                    .and_then(|snapshot| {
                        snapshot
                            .workspaces
                            .iter()
                            .find(|workspace| workspace.workspace_id == workspace_id)
                    })
                    .map(|workspace| workspace.label.clone());
                if let Some(label) = label {
                    self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                        title: "rename workspace",
                        input: TextEditor::new(&label, false),
                        target: ClientRenameTarget::Workspace { workspace_id },
                    }));
                }
            }
            ClientContextMenuAction::Close => {
                if self.config.confirm_close {
                    self.open_confirm_close_overlay(workspace_id);
                } else {
                    self.push_endpoint_command(
                        shepr_protocol::command::EndpointCommand::WorkspaceClose(
                            shepr_protocol::command::WorkspaceCloseParams { workspace_id },
                        ),
                        outcome,
                    );
                }
            }
            _ => {}
        }
    }

    fn activate_pane_context_action(
        &mut self,
        pane_id: shepr_protocol::PublicPaneId,
        source_pane_id: Option<shepr_protocol::PublicPaneId>,
        right_click_passthrough: bool,
        action: ClientContextMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        use shepr_protocol::command::{
            PaneInputSetParams, PaneRenameParams, PaneSplitParams, PaneTarget, PaneZoomParams,
        };

        match action {
            ClientContextMenuAction::RenamePane => {
                let label = self.snapshot.as_deref().and_then(|snapshot| {
                    snapshot
                        .panes
                        .iter()
                        .find(|pane| pane.pane_id == pane_id)
                        .and_then(|pane| pane.label.clone())
                });
                self.overlay = Some(ClientShellOverlay::Rename(ClientRenameOverlay {
                    title: "rename pane",
                    input: TextEditor::new(label.as_deref().unwrap_or_default(), label.is_none()),
                    target: ClientRenameTarget::Pane { pane_id },
                }));
            }
            ClientContextMenuAction::ClearPaneName => self.push_endpoint_command(
                EndpointCommand::PaneRename(PaneRenameParams {
                    pane_id,
                    label: None,
                }),
                outcome,
            ),
            ClientContextMenuAction::SwapWithFocusedPane => {
                if let Some(source_pane_id) = source_pane_id {
                    self.push_endpoint_command(
                        EndpointCommand::PaneSwap(PaneSwapParams::Panes {
                            source: source_pane_id,
                            target: pane_id,
                        }),
                        outcome,
                    );
                    self.push_endpoint_command(
                        EndpointCommand::PaneFocus(PaneTarget {
                            pane_id: source_pane_id,
                        }),
                        outcome,
                    );
                }
            }
            ClientContextMenuAction::SplitRight | ClientContextMenuAction::SplitDown => {
                self.push_endpoint_command(
                    EndpointCommand::PaneSplit(PaneSplitParams {
                        pane_id,
                        direction: if action == ClientContextMenuAction::SplitRight {
                            SplitDirection::Right
                        } else {
                            SplitDirection::Down
                        },
                    }),
                    outcome,
                );
            }
            ClientContextMenuAction::Zoom => self.push_endpoint_command(
                EndpointCommand::PaneZoom(PaneZoomParams { pane_id }),
                outcome,
            ),
            ClientContextMenuAction::ToggleRightClickPassthrough => self.push_endpoint_command(
                EndpointCommand::PaneInputSet(PaneInputSetParams {
                    pane_id,
                    right_click: if right_click_passthrough {
                        PaneRightClickTarget::Shepr
                    } else {
                        PaneRightClickTarget::Pane
                    },
                }),
                outcome,
            ),
            ClientContextMenuAction::ClosePane => {
                self.push_endpoint_command(
                    EndpointCommand::PaneClose(PaneTarget { pane_id }),
                    outcome,
                );
            }
            _ => {}
        }
    }
}
