//! The right-click context menu of a workspace or a pane: its target, items, layout and input,
//! and the shell work its actions do.

use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use shepr_protocol::command::EndpointCommand;
use shepr_protocol::command::PaneRightClickTarget;
use shepr_protocol::command::PaneSwapParams;
use shepr_protocol::command::SplitDirection;
use shepr_term::key::TerminalKey;

use super::rename::RenameOverlay;
use super::{MenuView, Overlay, OverlayCommand, OverlayContext, OverlayEffect, OverlayPaint};
use super::{draw_menu, menu_view};
use crate::shell::input::hit_test::contains;
use crate::shell::presentation::text::display_width;
use crate::shell::state::{ClientShellInput, ClientShellState};

/// Minimum context-menu width, before the screen width is applied.
const MIN_CONTEXT_MENU_WIDTH: u16 = 14;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::shell) enum ContextMenuAction {
    Rename,
    Close,
    RenamePane,
    ClearPaneName,
    SwapWithFocusedPane,
    SplitRight,
    SplitDown,
    Zoom,
    ToggleRightClickPassthrough,
    ClosePane,
}

#[derive(Clone, Debug)]
pub(in crate::shell) enum ContextMenuTarget {
    Workspace {
        workspace_id: shepr_protocol::WorkspaceId,
    },
    Pane {
        pane_id: shepr_protocol::PublicPaneId,
        source_pane_id: Option<shepr_protocol::PublicPaneId>,
        has_manual_label: bool,
        right_click_passthrough: bool,
    },
}

#[derive(Debug)]
pub(in crate::shell) struct ContextMenuOverlay {
    pub(in crate::shell) target: ContextMenuTarget,
    pub(in crate::shell) x: u16,
    pub(in crate::shell) y: u16,
    pub(in crate::shell) highlighted: usize,
}

pub(in crate::shell) struct ContextMenuItem {
    pub(in crate::shell) label: &'static str,
    pub(in crate::shell) action: ContextMenuAction,
}

impl ContextMenuOverlay {
    pub(in crate::shell) fn items(&self) -> Vec<ContextMenuItem> {
        use ContextMenuAction as Action;

        let item = |label, action| ContextMenuItem { label, action };
        match &self.target {
            ContextMenuTarget::Workspace { .. } => {
                vec![item("Rename", Action::Rename), item("Close", Action::Close)]
            }
            ContextMenuTarget::Pane {
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

    pub(super) fn layout(&self, screen: Rect) -> Option<MenuView> {
        let items = self.items();
        let max_item_width = items
            .iter()
            .map(|item| display_width(item.label))
            .max()
            .unwrap_or(0);
        let width = max_item_width
            .saturating_add(4)
            .max(MIN_CONTEXT_MENU_WIDTH)
            .min(screen.width.max(1));
        let height = u16::try_from(items.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .min(screen.height.max(1));
        let x = self
            .x
            .min(screen.x.saturating_add(screen.width.saturating_sub(width)));
        let y = self.y.min(
            screen
                .y
                .saturating_add(screen.height.saturating_sub(height)),
        );
        menu_view(Rect::new(x, y, width, height), items.len())
    }

    pub(super) fn draw(
        &self,
        buffer: &mut Buffer,
        view: &MenuView,
        ctx: &OverlayContext<'_>,
    ) -> OverlayPaint {
        let items = self.items();
        draw_menu(
            buffer,
            view,
            self.highlighted,
            |index| items.get(index).map(|item| item.label.to_owned()),
            ctx.palette,
        )
    }

    fn move_selection(&mut self, delta: isize) {
        let item_count = self.items().len();
        if item_count == 0 {
            return;
        }
        let max_index = item_count.saturating_sub(1);
        self.highlighted = self
            .highlighted
            .checked_add_signed(delta)
            .unwrap_or(0)
            .min(max_index);
    }

    /// Activating an index with no item closes the menu and does nothing else.
    fn activate(&self, index: usize) -> OverlayEffect {
        match self.items().get(index) {
            Some(item) => OverlayEffect::Command(OverlayCommand::ContextMenu {
                target: self.target.clone(),
                action: item.action,
            }),
            None => OverlayEffect::Close,
        }
    }

    pub(super) fn on_key(&mut self, key: &TerminalKey) -> OverlayEffect {
        match key.code {
            KeyCode::Esc => OverlayEffect::Close,
            KeyCode::Up => {
                self.move_selection(-1);
                OverlayEffect::Changed
            }
            KeyCode::Down => {
                self.move_selection(1);
                OverlayEffect::Changed
            }
            KeyCode::Enter => self.activate(self.highlighted),
            _ => OverlayEffect::Unchanged,
        }
    }

    pub(super) fn on_mouse(&mut self, mouse: MouseEvent, view: Option<&MenuView>) -> OverlayEffect {
        let point = (mouse.column, mouse.row);
        let row_hit = view.and_then(|view| {
            view.rows
                .iter()
                .find(|(rect, _)| contains(*rect, point))
                .map(|(_, index)| *index)
        });
        match mouse.kind {
            MouseEventKind::Moved => match row_hit {
                Some(index) => {
                    self.highlighted = index;
                    OverlayEffect::Changed
                }
                None => OverlayEffect::Unchanged,
            },
            MouseEventKind::Down(MouseButton::Left) => match row_hit {
                Some(index) => self.activate(index),
                None => OverlayEffect::Close,
            },
            _ => OverlayEffect::Unchanged,
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
        let Some(snapshot) = self.endpoints.active.snapshot() else {
            return;
        };
        if !snapshot
            .workspaces
            .iter()
            .any(|workspace| workspace.workspace_id == workspace_id)
        {
            return;
        }
        self.overlay = Some(Overlay::ContextMenu(ContextMenuOverlay {
            target: ContextMenuTarget::Workspace { workspace_id },
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
        let Some(snapshot) = self.endpoints.active.snapshot() else {
            return;
        };
        let Some(pane) = snapshot.panes.iter().find(|pane| pane.pane_id == pane_id) else {
            return;
        };
        let source_pane_id = snapshot
            .focused_pane_id
            .filter(|focused| focused != &pane_id);
        self.overlay = Some(Overlay::ContextMenu(ContextMenuOverlay {
            target: ContextMenuTarget::Pane {
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

    /// Does what a chosen menu item says. The menu is already closed.
    pub(in crate::shell) fn activate_context_menu_action(
        &mut self,
        target: &ContextMenuTarget,
        action: ContextMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        match *target {
            ContextMenuTarget::Workspace { workspace_id, .. } => {
                self.activate_workspace_context_action(workspace_id, action, outcome);
            }
            ContextMenuTarget::Pane {
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
        action: ContextMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        match action {
            ContextMenuAction::Rename => {
                let label = self
                    .endpoints
                    .active
                    .snapshot()
                    .and_then(|snapshot| {
                        snapshot
                            .workspaces
                            .iter()
                            .find(|workspace| workspace.workspace_id == workspace_id)
                    })
                    .map(|workspace| workspace.label.clone());
                if let Some(label) = label {
                    self.overlay = Some(Overlay::Rename(RenameOverlay::workspace(
                        workspace_id,
                        &label,
                    )));
                }
            }
            ContextMenuAction::Close => {
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
        action: ContextMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        use shepr_protocol::command::{
            PaneInputSetParams, PaneRenameParams, PaneSplitParams, PaneTarget, PaneZoomParams,
        };

        match action {
            ContextMenuAction::RenamePane => {
                let label = self.endpoints.active.snapshot().and_then(|snapshot| {
                    snapshot
                        .panes
                        .iter()
                        .find(|pane| pane.pane_id == pane_id)
                        .and_then(|pane| pane.label.clone())
                });
                self.overlay = Some(Overlay::Rename(RenameOverlay::pane(
                    pane_id,
                    label.as_deref(),
                )));
            }
            ContextMenuAction::ClearPaneName => self.push_endpoint_command(
                EndpointCommand::PaneRename(PaneRenameParams {
                    pane_id,
                    label: None,
                }),
                outcome,
            ),
            ContextMenuAction::SwapWithFocusedPane => {
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
            ContextMenuAction::SplitRight | ContextMenuAction::SplitDown => {
                self.push_endpoint_command(
                    EndpointCommand::PaneSplit(PaneSplitParams {
                        pane_id,
                        direction: if action == ContextMenuAction::SplitRight {
                            SplitDirection::Right
                        } else {
                            SplitDirection::Down
                        },
                    }),
                    outcome,
                );
            }
            ContextMenuAction::Zoom => self.push_endpoint_command(
                EndpointCommand::PaneZoom(PaneZoomParams { pane_id }),
                outcome,
            ),
            ContextMenuAction::ToggleRightClickPassthrough => self.push_endpoint_command(
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
            ContextMenuAction::ClosePane => {
                self.push_endpoint_command(
                    EndpointCommand::PaneClose(PaneTarget { pane_id }),
                    outcome,
                );
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ContextMenuAction, ContextMenuOverlay, ContextMenuTarget};
    use crate::tests::{test_pane_id, test_workspace_id};

    fn menu(target: ContextMenuTarget) -> ContextMenuOverlay {
        ContextMenuOverlay {
            target,
            x: 0,
            y: 0,
            highlighted: 0,
        }
    }

    fn pane(
        source: bool,
        has_manual_label: bool,
        right_click_passthrough: bool,
    ) -> ContextMenuOverlay {
        menu(ContextMenuTarget::Pane {
            pane_id: test_pane_id("w1:p1"),
            source_pane_id: source.then(|| test_pane_id("w1:p2")),
            has_manual_label,
            right_click_passthrough,
        })
    }

    fn labels(menu: &ContextMenuOverlay) -> Vec<&'static str> {
        menu.items().iter().map(|item| item.label).collect()
    }

    #[test]
    fn items_follow_the_target() {
        let workspace = menu(ContextMenuTarget::Workspace {
            workspace_id: test_workspace_id("w1"),
        });
        assert_eq!(labels(&workspace), ["Rename", "Close"]);
        assert_eq!(workspace.items()[1].action, ContextMenuAction::Close);

        assert_eq!(
            labels(&pane(false, false, false)),
            [
                "Rename pane",
                "Split right",
                "Split down",
                "Zoom",
                "Send right-clicks to pane",
                "Close pane"
            ]
        );
        assert_eq!(
            labels(&pane(true, true, true)),
            [
                "Rename pane",
                "Clear pane name",
                "Swap with focused pane",
                "Split right",
                "Split down",
                "Zoom",
                "Use Shepr right-click menu",
                "Close pane"
            ]
        );
    }
}
