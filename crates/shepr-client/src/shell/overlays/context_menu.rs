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
use crate::limits::MIN_CONTEXT_MENU_WIDTH;
use crate::shell::input::hit_test::contains;
use crate::shell::presentation::text::display_width;
use crate::shell::state::{ClientShellInput, ClientShellState};

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
        /// Whether the pane's workspace is zoomed.
        zoomed: bool,
        /// Whether the pane's workspace has more than one pane; a lone pane
        /// has nothing to zoom over.
        can_zoom: bool,
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
    /// A disabled item is drawn muted and neither highlights nor activates.
    pub(in crate::shell) enabled: bool,
}

impl ContextMenuOverlay {
    pub(in crate::shell) fn items(&self) -> Vec<ContextMenuItem> {
        use ContextMenuAction as Action;

        let item = |label, action| ContextMenuItem {
            label,
            action,
            enabled: true,
        };
        match &self.target {
            ContextMenuTarget::Workspace { .. } => {
                vec![item("Rename", Action::Rename), item("Close", Action::Close)]
            }
            ContextMenuTarget::Pane {
                source_pane_id,
                has_manual_label,
                right_click_passthrough,
                zoomed,
                can_zoom,
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
                    ContextMenuItem {
                        enabled: *can_zoom,
                        ..item(if *zoomed { "Zoom out" } else { "Zoom in" }, Action::Zoom)
                    },
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
            |index| items.get(index).is_none_or(|item| item.enabled),
            ctx.palette,
        )
    }

    fn move_selection(&mut self, delta: isize) {
        let items = self.items();
        if items.is_empty() {
            return;
        }
        let max_index = items.len().saturating_sub(1);
        let mut index = self.highlighted;
        // Steps over disabled items; at either end the highlight stays put.
        loop {
            let Some(next) = index
                .checked_add_signed(delta)
                .filter(|next| *next <= max_index)
            else {
                return;
            };
            index = next;
            if items[index].enabled {
                self.highlighted = index;
                return;
            }
        }
    }

    /// Activating an index with no item closes the menu and does nothing else;
    /// a disabled item does nothing and leaves the menu open.
    fn activate(&self, index: usize) -> OverlayEffect {
        match self.items().get(index) {
            Some(item) if !item.enabled => OverlayEffect::Unchanged,
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
                Some(index) if self.items().get(index).is_some_and(|item| item.enabled) => {
                    self.highlighted = index;
                    OverlayEffect::Changed
                }
                _ => OverlayEffect::Unchanged,
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
                zoomed: snapshot.workspaces.iter().any(|workspace| {
                    workspace.workspace_id == *pane_id.workspace_id() && workspace.zoomed
                }),
                can_zoom: snapshot
                    .panes
                    .iter()
                    .filter(|other| other.pane_id.workspace_id() == pane_id.workspace_id())
                    .count()
                    > 1,
            },
            x,
            y,
            highlighted: 0,
        }));
    }

    /// Does what a chosen menu item says. The menu is already closed.
    pub(super) fn activate_context_menu_action(
        &mut self,
        target: &ContextMenuTarget,
        action: ContextMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        match *target {
            ContextMenuTarget::Workspace { workspace_id, .. } => {
                self.activate_workspace_context_action(workspace_id, action);
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
                self.open_confirm_close_overlay(workspace_id);
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
    use crate::shell::config::ClientShellConfig;
    use crate::shell::overlays::OverlayEffect;
    use crate::shell::state::{ClientShellAction, ClientShellState};
    use crate::shell::tests::{press, snapshot};
    use crate::tests::{test_pane_id, test_workspace_id};
    use crossterm::event::KeyModifiers;
    use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;
    use shepr_config::ClientConfig;
    use shepr_term::key::TerminalKey;

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
            zoomed: false,
            can_zoom: true,
        })
    }

    fn zoom_menu(zoomed: bool, can_zoom: bool) -> ContextMenuOverlay {
        menu(ContextMenuTarget::Pane {
            pane_id: test_pane_id("w1:p1"),
            source_pane_id: None,
            has_manual_label: false,
            right_click_passthrough: false,
            zoomed,
            can_zoom,
        })
    }

    fn zoom_index(menu: &ContextMenuOverlay) -> usize {
        menu.items()
            .iter()
            .position(|item| item.action == ContextMenuAction::Zoom)
            .expect("zoom item")
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
                "Zoom in",
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
                "Zoom in",
                "Use Shepr right-click menu",
                "Close pane"
            ]
        );
    }

    #[test]
    fn the_zoom_item_names_the_toggle_it_does() {
        let unzoomed = zoom_menu(false, true);
        let item = &unzoomed.items()[zoom_index(&unzoomed)];
        assert_eq!(item.label, "Zoom in");
        assert!(item.enabled);

        let zoomed = zoom_menu(true, true);
        let item = &zoomed.items()[zoom_index(&zoomed)];
        assert_eq!(item.label, "Zoom out");
        assert!(item.enabled);
    }

    #[test]
    fn a_lone_pane_has_a_disabled_zoom_item() {
        let mut menu = zoom_menu(false, false);
        let index = zoom_index(&menu);
        assert!(!menu.items()[index].enabled);
        assert_eq!(menu.items().iter().filter(|item| !item.enabled).count(), 1);

        // Enter on it does nothing and keeps the menu open.
        menu.highlighted = index;
        assert!(matches!(menu.activate(index), OverlayEffect::Unchanged));
        assert!(matches!(
            menu.on_key(&TerminalKey::new(
                KeyCode::Enter,
                crossterm::event::KeyModifiers::empty()
            )),
            OverlayEffect::Unchanged
        ));
    }

    #[test]
    fn keyboard_navigation_skips_a_disabled_item() {
        let mut menu = zoom_menu(false, false);
        let index = zoom_index(&menu);
        menu.highlighted = index - 1;
        menu.move_selection(1);
        assert_eq!(menu.highlighted, index + 1);
        menu.move_selection(-1);
        assert_eq!(menu.highlighted, index - 1);

        // The ends hold the highlight.
        menu.highlighted = 0;
        menu.move_selection(-1);
        assert_eq!(menu.highlighted, 0);
        let last = menu.items().len() - 1;
        menu.highlighted = last;
        menu.move_selection(1);
        assert_eq!(menu.highlighted, last);
    }

    #[test]
    fn hovering_or_clicking_a_disabled_item_changes_nothing() {
        let mut menu = zoom_menu(false, false);
        let index = zoom_index(&menu);
        let view = menu
            .layout(Rect::new(0, 0, 80, 24))
            .expect("menu fits the screen");
        let row = view
            .rows
            .iter()
            .find(|(_, i)| *i == index)
            .expect("zoom row")
            .0;
        let event = |kind| MouseEvent {
            kind,
            column: row.x + 1,
            row: row.y,
            modifiers: KeyModifiers::empty(),
        };
        assert!(matches!(
            menu.on_mouse(event(MouseEventKind::Moved), Some(&view)),
            OverlayEffect::Unchanged
        ));
        assert_eq!(menu.highlighted, 0);
        assert!(matches!(
            menu.on_mouse(event(MouseEventKind::Down(MouseButton::Left)), Some(&view)),
            OverlayEffect::Unchanged
        ));
    }

    fn opened_menu(state: &mut ClientShellState) -> ContextMenuOverlay {
        state.open_pane_context_menu(test_pane_id("w1:p1"), 0, 0);
        match state.overlay.take() {
            Some(crate::shell::overlays::Overlay::ContextMenu(menu)) => menu,
            _ => panic!("pane context menu"),
        }
    }

    #[test]
    fn the_menu_reads_the_zoom_and_pane_count_from_the_projection() {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        let mut projection = snapshot();
        state.set_snapshot(Box::new(projection.clone()));
        let lone = opened_menu(&mut state);
        assert!(!lone.items()[zoom_index(&lone)].enabled);

        let mut second = projection.panes[0].clone();
        second.pane_id = test_pane_id("w1:p2");
        projection.panes.push(second);
        state.set_snapshot(Box::new(projection.clone()));
        let split = opened_menu(&mut state);
        let item = &split.items()[zoom_index(&split)];
        assert!(item.enabled);
        assert_eq!(item.label, "Zoom in");

        projection.workspaces[0].zoomed = true;
        state.set_snapshot(Box::new(projection));
        let zoomed = opened_menu(&mut state);
        assert_eq!(zoomed.items()[zoom_index(&zoomed)].label, "Zoom out");
    }

    #[test]
    fn the_zoom_key_sends_nothing_for_a_lone_pane() {
        use shepr_protocol::command::EndpointCommand;

        let zoom_requests = |state: &mut ClientShellState| {
            let prefix = state.config.keybinds.prefix;
            press(state, prefix.code, prefix.modifiers);
            let outcome = press(state, KeyCode::Char('z'), KeyModifiers::empty());
            outcome
                .actions
                .iter()
                .filter(|action| {
                    matches!(
                        action,
                        ClientShellAction::Endpoint { request, .. }
                            if matches!(request.command, EndpointCommand::PaneZoom(_))
                    )
                })
                .count()
        };
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        let mut projection = snapshot();
        state.set_snapshot(Box::new(projection.clone()));
        assert_eq!(zoom_requests(&mut state), 0);

        let mut second = projection.panes[0].clone();
        second.pane_id = test_pane_id("w1:p2");
        projection.panes.push(second);
        state.set_snapshot(Box::new(projection));
        assert_eq!(zoom_requests(&mut state), 1);
    }
}
