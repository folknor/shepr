//! The modal overlays. Each lives in its own module with its state, layout, drawing and key,
//! mouse and paste handling, and none calls a `ClientShellState` method: input returns an
//! `OverlayEffect` that `apply_overlay_effect` applies here, together with the openers that
//! need the snapshot and the request ledger. Typed and pasted text land in overlay editors;
//! input content must stay out of logs and error messages here (log lengths or content-free
//! kinds instead).

use crate::shell::palette::Palette;
use crossterm::event::MouseEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use shepr_config::LiveKeybindConfig;
use shepr_term::key::TerminalKey;
use shepr_term::scroll::ListScroll;

pub(in crate::shell) mod confirm_close;
pub(in crate::shell) mod confirm_restart;
pub(in crate::shell) mod context_menu;
pub(in crate::shell) mod global_menu;
pub(in crate::shell) mod help;
pub(in crate::shell) mod navigator;
pub(in crate::shell) mod rename;
pub(in crate::shell) mod text_editor;
mod widgets;

use crate::endpoint::ClientEndpointId;
use crate::shell::navigation::aggregate_navigation::NavigatorIndex;
use crate::shell::navigation::location::{Location, LocationTarget};
use crate::shell::presentation::status::panel_contrast_fg;
use crate::shell::presentation::text::put_text;
use crate::shell::state::{ClientShellInput, ClientShellMode, ClientShellState};
use crate::shell::view::list::ListView;
use crate::shell::view::resolve::overlay_context;
use confirm_close::ConfirmCloseOverlay;
use confirm_restart::ConfirmRestartOverlay;
use context_menu::{ContextMenuAction, ContextMenuOverlay, ContextMenuTarget};
use global_menu::{GlobalMenuAction, GlobalMenuOverlay};
use help::HelpOverlay;
use navigator::{ClientNavigatorRow, NavigatorOverlay};
use rename::{RenameOverlay, RenameTarget};
use widgets::panel;

#[derive(Debug)]
pub(in crate::shell) enum Overlay {
    Rename(RenameOverlay),
    ConfirmClose(ConfirmCloseOverlay),
    ConfirmRestart(ConfirmRestartOverlay),
    Help(HelpOverlay),
    Navigator(NavigatorOverlay),
    ContextMenu(ContextMenuOverlay),
    GlobalMenu(GlobalMenuOverlay),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::shell) enum OverlayKind {
    Rename,
    ConfirmClose,
    ConfirmRestart,
    Help,
    Navigator,
    ContextMenu,
    GlobalMenu,
}

/// The open overlay as laid out for one frame. Each variant carries only its own overlay's
/// geometry, so no field can belong to another overlay.
pub(in crate::shell) enum OverlayView {
    Rename(DialogView),
    ConfirmClose(DialogView),
    ConfirmRestart(DialogView),
    Help(HelpView),
    Navigator(NavigatorView),
    ContextMenu(MenuView),
    GlobalMenu(MenuView),
}

pub(in crate::shell) struct DialogView {
    pub(in crate::shell) popup: Rect,
    pub(in crate::shell) inner: Rect,
    /// Rename only.
    pub(in crate::shell) input: Option<Rect>,
    pub(in crate::shell) primary: Rect,
    /// Rename only.
    pub(in crate::shell) clear: Option<Rect>,
    cancel: Rect,
}

pub(in crate::shell) struct HelpView {
    pub(in crate::shell) popup: Rect,
    pub(in crate::shell) inner: Rect,
    pub(in crate::shell) close: Rect,
    text_area: Rect,
    pub(in crate::shell) scroll: ListScroll,
    pub(in crate::shell) scrollbar: Option<Rect>,
}

pub(in crate::shell) struct NavigatorView {
    pub(in crate::shell) popup: Rect,
    pub(in crate::shell) inner: Rect,
    pub(in crate::shell) search: Rect,
    pub(in crate::shell) body: Rect,
    /// Computed once per frame.
    pub(in crate::shell) rows: Vec<ClientNavigatorRow>,
    pub(in crate::shell) selected: usize,
    pub(in crate::shell) list: ListView<NavigatorSlot>,
}

pub(in crate::shell) struct NavigatorSlot {
    pub(in crate::shell) rect: Rect,
    pub(in crate::shell) row: usize,
}

pub(in crate::shell) struct MenuView {
    pub(in crate::shell) rect: Rect,
    pub(in crate::shell) rows: Vec<(Rect, usize)>,
}

/// The scroll position a laid-out overlay resolved, for the commit step to store.
pub(in crate::shell) enum OverlayScroll {
    Navigator(usize),
    Help(usize),
}

/// What drawing an overlay committed beyond its cells.
///
/// Overlays draw into a fresh full-screen scratch buffer, never into the frame; composition
/// commits only a successful layout: the `backdrop` dimming first, then the scratch cells of
/// the `opaque` rects (the union is one replacement).
#[derive(Default)]
pub(in crate::shell) struct OverlayPaint {
    /// Absolute rects the overlay painted opaquely: every scratch cell it drew is inside one.
    pub(in crate::shell) opaque: Vec<Rect>,
    /// Whether the whole frame is dimmed behind the overlay (Help, Rename and the
    /// confirmations).
    pub(in crate::shell) backdrop: bool,
    pub(in crate::shell) cursor: Option<shepr_protocol::CursorState>,
}

/// What laying out, drawing and handling input for an overlay reads besides the overlay itself.
pub(in crate::shell) struct OverlayContext<'a> {
    pub(in crate::shell) navigator_index: &'a NavigatorIndex,
    pub(in crate::shell) active_endpoint_id: &'a ClientEndpointId,
    pub(in crate::shell) keybinds: &'a LiveKeybindConfig,
    pub(in crate::shell) palette: &'a Palette,
    /// The sidebar's menu launcher on the frame on screen, for the global menu's toggle click.
    pub(in crate::shell) global_launcher: Option<Rect>,
}

/// What an overlay's input did.
pub(in crate::shell::overlays) enum OverlayEffect {
    /// Consumed, nothing changed.
    Unchanged,
    /// Consumed; the overlay changed and the frame repaints.
    Changed,
    /// Close the overlay and repaint.
    Close,
    /// Shell work; the overlay stays open unless the command closes it.
    Command(OverlayCommand),
}

pub(in crate::shell::overlays) enum OverlayCommand {
    /// Activate the navigator's target; the overlay closes only if activation succeeds.
    OpenTarget(Location),
    /// Rename or create; the overlay closes.
    SaveRename {
        target: RenameTarget,
        label: Option<String>,
    },
    CloseWorkspace(shepr_protocol::WorkspaceId),
    /// Esc on a confirmation; back to Navigate when it came from there.
    CancelClose {
        return_to_navigate: bool,
    },
    /// The Restart question was answered yes; the overlay closes.
    RestartMachine(ClientEndpointId),
    GlobalMenu(GlobalMenuAction),
    ContextMenu {
        target: ContextMenuTarget,
        action: ContextMenuAction,
    },
    /// The global menu's launcher was pressed while the menu is open.
    ToggleGlobalMenu,
}

impl Overlay {
    pub(in crate::shell) fn kind(&self) -> OverlayKind {
        match self {
            Self::Rename(_) => OverlayKind::Rename,
            Self::ConfirmClose(_) => OverlayKind::ConfirmClose,
            Self::ConfirmRestart(_) => OverlayKind::ConfirmRestart,
            Self::Help(_) => OverlayKind::Help,
            Self::Navigator(_) => OverlayKind::Navigator,
            Self::ContextMenu(_) => OverlayKind::ContextMenu,
            Self::GlobalMenu(_) => OverlayKind::GlobalMenu,
        }
    }

    /// Lays the overlay out on `screen`. `None` when it does not fit: nothing is drawn then.
    pub(in crate::shell) fn layout(
        &self,
        screen: Rect,
        ctx: &OverlayContext<'_>,
    ) -> Option<(OverlayView, Option<OverlayScroll>)> {
        match self {
            Self::Rename(_) => {
                RenameOverlay::layout(screen).map(|view| (OverlayView::Rename(view), None))
            }
            Self::ConfirmClose(_) => ConfirmCloseOverlay::layout(screen)
                .map(|view| (OverlayView::ConfirmClose(view), None)),
            Self::ConfirmRestart(_) => ConfirmRestartOverlay::layout(screen)
                .map(|view| (OverlayView::ConfirmRestart(view), None)),
            Self::Help(help) => help
                .layout(screen, ctx)
                .map(|(view, scroll)| (OverlayView::Help(view), Some(scroll))),
            Self::Navigator(navigator) => navigator
                .layout(screen, ctx)
                .map(|(view, scroll)| (OverlayView::Navigator(view), Some(scroll))),
            Self::ContextMenu(menu) => menu
                .layout(screen)
                .map(|view| (OverlayView::ContextMenu(view), None)),
            Self::GlobalMenu(menu) => menu
                .layout(screen)
                .map(|view| (OverlayView::GlobalMenu(view), None)),
        }
    }

    /// Draws the overlay into `buffer` from its view. A view of another overlay's kind draws
    /// nothing.
    pub(in crate::shell) fn draw(
        &self,
        view: &OverlayView,
        buffer: &mut Buffer,
        ctx: &OverlayContext<'_>,
    ) -> OverlayPaint {
        match (self, view) {
            (Self::Rename(rename), OverlayView::Rename(view)) => {
                rename.draw(buffer, view, ctx.palette)
            }
            (Self::ConfirmClose(confirm), OverlayView::ConfirmClose(view)) => {
                confirm.draw(buffer, view, ctx.palette)
            }
            (Self::ConfirmRestart(confirm), OverlayView::ConfirmRestart(view)) => {
                confirm.draw(buffer, view, ctx.palette)
            }
            (Self::Help(help), OverlayView::Help(view)) => help.draw(buffer, view, ctx),
            (Self::Navigator(navigator), OverlayView::Navigator(view)) => {
                navigator.draw(buffer, view, ctx)
            }
            (Self::ContextMenu(menu), OverlayView::ContextMenu(view)) => {
                menu.draw(buffer, view, ctx)
            }
            (Self::GlobalMenu(menu), OverlayView::GlobalMenu(view)) => menu.draw(buffer, view, ctx),
            _ => OverlayPaint::default(),
        }
    }

    /// Handles a key. `view` is the overlay as last drawn; one of another kind, or none, means
    /// the overlay is not on screen.
    fn on_key(
        &mut self,
        key: &TerminalKey,
        view: Option<&OverlayView>,
        ctx: &OverlayContext<'_>,
    ) -> OverlayEffect {
        match self {
            Self::Rename(rename) => rename.on_key(key),
            Self::ConfirmClose(confirm) => confirm.on_key(key),
            Self::ConfirmRestart(confirm) => confirm.on_key(key),
            Self::Help(help) => help.on_key(
                key,
                match view {
                    Some(OverlayView::Help(view)) => Some(view),
                    _ => None,
                },
            ),
            Self::Navigator(navigator) => navigator.on_key(key, ctx),
            Self::ContextMenu(menu) => menu.on_key(key),
            Self::GlobalMenu(menu) => menu.on_key(key),
        }
    }

    /// Handles a mouse event. `view` is as for `on_key`; a press with no view of this
    /// overlay's kind is handled as a press outside the popup.
    fn on_mouse(
        &mut self,
        mouse: MouseEvent,
        view: Option<&OverlayView>,
        ctx: &OverlayContext<'_>,
    ) -> OverlayEffect {
        match self {
            Self::Rename(rename) => rename.on_mouse(
                mouse,
                match view {
                    Some(OverlayView::Rename(view)) => Some(view),
                    _ => None,
                },
            ),
            Self::ConfirmClose(confirm) => confirm.on_mouse(
                mouse,
                match view {
                    Some(OverlayView::ConfirmClose(view)) => Some(view),
                    _ => None,
                },
            ),
            Self::ConfirmRestart(confirm) => confirm.on_mouse(
                mouse,
                match view {
                    Some(OverlayView::ConfirmRestart(view)) => Some(view),
                    _ => None,
                },
            ),
            Self::Help(help) => help.on_mouse(
                mouse,
                match view {
                    Some(OverlayView::Help(view)) => Some(view),
                    _ => None,
                },
            ),
            Self::Navigator(navigator) => navigator.on_mouse(
                mouse,
                match view {
                    Some(OverlayView::Navigator(view)) => Some(view),
                    _ => None,
                },
                ctx,
            ),
            Self::ContextMenu(menu) => menu.on_mouse(
                mouse,
                match view {
                    Some(OverlayView::ContextMenu(view)) => Some(view),
                    _ => None,
                },
            ),
            Self::GlobalMenu(menu) => menu.on_mouse(
                mouse,
                match view {
                    Some(OverlayView::GlobalMenu(view)) => Some(view),
                    _ => None,
                },
                ctx.global_launcher,
            ),
        }
    }

    /// Pasted text; whether the overlay took it.
    fn on_text(&mut self, text: &str) -> bool {
        match self {
            Self::Rename(rename) => rename.on_text(text),
            Self::Help(help) => help.on_text(text),
            Self::Navigator(navigator) => navigator.on_text(text),
            Self::ConfirmClose(_)
            | Self::ConfirmRestart(_)
            | Self::ContextMenu(_)
            | Self::GlobalMenu(_) => false,
        }
    }

    /// Whether the clipboard-paste shortcut pastes into this overlay.
    pub(in crate::shell) fn accepts_modal_paste(&self) -> bool {
        match self {
            Self::Rename(_) => true,
            Self::Help(help) => help.accepts_modal_paste(),
            Self::Navigator(navigator) => navigator.accepts_modal_paste(),
            Self::ConfirmClose(_)
            | Self::ConfirmRestart(_)
            | Self::ContextMenu(_)
            | Self::GlobalMenu(_) => false,
        }
    }

    /// Stores the scroll position the frame resolved. A scroll of another overlay's kind is
    /// ignored.
    pub(in crate::shell) fn commit_scroll(&mut self, scroll: OverlayScroll) {
        match (self, scroll) {
            (Self::Navigator(navigator), OverlayScroll::Navigator(start)) => {
                navigator.scroll = start;
            }
            (Self::Help(help), OverlayScroll::Help(start)) => help.scroll = start,
            _ => {}
        }
    }
}

/// Lays a menu's rows out inside its panel.
fn menu_view(rect: Rect, item_count: usize) -> Option<MenuView> {
    let inner = widgets::panel_inner(rect)?;
    let mut rows = Vec::new();
    for index in 0..item_count {
        let row_y = inner
            .y
            .saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
        if row_y >= inner.bottom() {
            break;
        }
        rows.push((Rect::new(inner.x, row_y, inner.width, 1), index));
    }
    Some(MenuView { rect, rows })
}

/// Draws a menu's panel and rows. `label` gives the text of the item at an index.
fn draw_menu(
    buffer: &mut Buffer,
    view: &MenuView,
    highlighted_index: usize,
    label: impl Fn(usize) -> Option<String>,
    enabled: impl Fn(usize) -> bool,
    palette: &Palette,
) -> OverlayPaint {
    panel(buffer, view.rect, palette.accent, palette.panel_bg);
    for (row, index) in &view.rows {
        let highlighted = *index == highlighted_index;
        let style = if !enabled(*index) {
            // A disabled item is muted and never drawn as the selection.
            Style::default().fg(palette.overlay0).bg(palette.panel_bg)
        } else if highlighted {
            Style::default()
                .fg(panel_contrast_fg(palette))
                .bg(palette.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette.text).bg(palette.panel_bg)
        };
        buffer.set_style(*row, style);
        if let Some(text) = label(*index) {
            put_text(buffer, row.x, row.y, row.width, &text, style);
        }
    }
    OverlayPaint {
        opaque: vec![view.rect],
        ..OverlayPaint::default()
    }
}

impl ClientShellState {
    /// Routes a key to the open overlay and applies what it asks for.
    pub(in crate::shell) fn route_overlay_key(
        &mut self,
        key: &TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        let Some(mut overlay) = self.overlay.take() else {
            return;
        };
        let effect = {
            let ctx = overlay_context(self);
            overlay.on_key(key, self.presentation.shown().overlay.as_ref(), &ctx)
        };
        self.overlay = Some(overlay);
        self.apply_overlay_effect(effect, outcome);
    }

    /// Routes a mouse event to the open overlay and applies what it asks for. An open overlay
    /// takes every mouse event that reaches it.
    pub(in crate::shell) fn route_overlay_mouse(
        &mut self,
        mouse: MouseEvent,
        outcome: &mut ClientShellInput,
    ) {
        let Some(mut overlay) = self.overlay.take() else {
            return;
        };
        let effect = {
            let ctx = overlay_context(self);
            overlay.on_mouse(mouse, self.presentation.shown().overlay.as_ref(), &ctx)
        };
        self.overlay = Some(overlay);
        self.apply_overlay_effect(effect, outcome);
    }

    fn apply_overlay_effect(&mut self, effect: OverlayEffect, outcome: &mut ClientShellInput) {
        match effect {
            OverlayEffect::Unchanged => {}
            OverlayEffect::Changed => outcome.repaint = true,
            OverlayEffect::Close => {
                self.overlay = None;
                outcome.repaint = true;
            }
            OverlayEffect::Command(command) => self.apply_overlay_command(command, outcome),
        }
    }

    fn apply_overlay_command(&mut self, command: OverlayCommand, outcome: &mut ClientShellInput) {
        match command {
            OverlayCommand::OpenTarget(target) => {
                if target.target == LocationTarget::Machine
                    && self.machine_entry_action(&target.endpoint).is_some()
                {
                    // Opening a machine that offers an action does what its
                    // entry does; a confirmation question replaces the navigator.
                    self.overlay = None;
                    self.activate_machine_entry(&target.endpoint, outcome);
                    outcome.repaint = true;
                    return;
                }
                let activated = match target.target {
                    LocationTarget::Machine => {
                        self.activate_endpoint(target.endpoint.clone(), outcome)
                    }
                    LocationTarget::Workspace(_) | LocationTarget::Pane(_) => {
                        self.focus_or_activate(target, outcome)
                    }
                };
                if activated {
                    self.overlay = None;
                }
                outcome.repaint = true;
            }
            OverlayCommand::SaveRename { target, label } => {
                self.overlay = None;
                self.push_endpoint_command(target.into_command(label), outcome);
                outcome.repaint = true;
            }
            OverlayCommand::CloseWorkspace(workspace_id) => {
                self.overlay = None;
                outcome.repaint = true;
                self.push_endpoint_command(
                    shepr_protocol::command::EndpointCommand::WorkspaceClose(
                        shepr_protocol::command::WorkspaceCloseParams { workspace_id },
                    ),
                    outcome,
                );
            }
            OverlayCommand::CancelClose { return_to_navigate } => {
                self.overlay = None;
                if return_to_navigate {
                    let preview = self.focused_navigation_target();
                    self.mode.enter_navigate(preview);
                    self.sidebar_scroll.reveal_selected_workspace();
                }
                outcome.repaint = true;
            }
            OverlayCommand::RestartMachine(endpoint_id) => {
                self.overlay = None;
                // The question was asked for this state; a machine that left it while
                // the question was open restarts nothing.
                if self.machine_entry_action(&endpoint_id)
                    == Some(crate::shell::endpoints::MachineAction::Restart)
                {
                    outcome
                        .actions
                        .push(crate::shell::state::ClientShellAction::RestartMachine(
                            endpoint_id,
                        ));
                }
                outcome.repaint = true;
            }
            OverlayCommand::GlobalMenu(action) => {
                self.overlay = None;
                self.activate_global_menu_action(action, outcome);
            }
            OverlayCommand::ContextMenu { target, action } => {
                self.overlay = None;
                self.activate_context_menu_action(&target, action, outcome);
            }
            OverlayCommand::ToggleGlobalMenu => {
                self.toggle_global_menu();
                outcome.repaint = true;
            }
        }
    }

    /// Pasted and modal-paste text for the open overlay; whether it took it.
    pub(in crate::shell) fn insert_overlay_text(&mut self, text: &str) -> bool {
        self.overlay
            .as_mut()
            .is_some_and(|overlay| overlay.on_text(text))
    }

    pub(in crate::shell) fn open_navigator_overlay(&mut self) {
        let mut navigator = NavigatorOverlay::default();
        let rows = self
            .endpoints
            .navigator_index
            .rows(self.endpoints.presented(), &navigator);
        navigator.selected = rows
            .iter()
            .find(|row| row.current)
            .map(|row| row.target.clone());
        self.overlay = Some(Overlay::Navigator(navigator));
    }

    /// The workspace a workspace action applies to: the focused one. Navigate
    /// mode's selection never is, since navigate mode runs no workspace actions.
    pub(in crate::shell) fn workspace_action_id(&self) -> Option<shepr_protocol::WorkspaceId> {
        self.endpoints
            .active
            .snapshot()
            .and_then(|snapshot| snapshot.focused_workspace_id)
    }

    /// Opens the new-workspace name prompt, prefilled with the name of the
    /// directory the workspace will start in. With no directory known it starts
    /// empty, and the server names the workspace after the one it picks. The
    /// heading names the presented machine, which the workspace is created on.
    pub(in crate::shell) fn open_new_workspace_overlay(&mut self) {
        let source_workspace_id = self.workspace_action_id();
        let cwd = self.endpoints.active.snapshot().and_then(|snapshot| {
            let workspace_id = source_workspace_id.as_ref()?;
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == *workspace_id)
                .and_then(|workspace| workspace.new_workspace_cwd.clone())
        });
        let suggested_name = cwd.as_ref().map_or_else(String::new, |cwd| {
            shepr_core::workspace_label::default_workspace_label(cwd.as_path())
        });
        let machine = self
            .endpoints
            .presented()
            .display_label(&self.config.local_label);
        self.overlay = Some(Overlay::Rename(RenameOverlay::new_workspace(
            cwd,
            &suggested_name,
            machine,
        )));
    }

    pub(in crate::shell) fn open_rename_workspace_overlay(&mut self) {
        let Some(snapshot) = self.endpoints.active.snapshot() else {
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
        self.overlay = Some(Overlay::Rename(RenameOverlay::workspace(
            workspace_id,
            &workspace.label,
        )));
    }

    pub(in crate::shell) fn open_rename_pane_overlay(&mut self) {
        let Some(snapshot) = self.endpoints.active.snapshot() else {
            return;
        };
        let Some(pane_id) = snapshot.focused_pane_id.as_ref() else {
            return;
        };
        let Some(pane) = snapshot.panes.iter().find(|pane| pane.pane_id == *pane_id) else {
            return;
        };
        self.overlay = Some(Overlay::Rename(RenameOverlay::pane(
            pane.pane_id,
            pane.label.as_deref(),
        )));
    }

    /// Opens the question a configured machine's Restart asks before anything is
    /// stopped.
    pub(in crate::shell) fn open_confirm_restart_overlay(
        &mut self,
        endpoint_id: &ClientEndpointId,
    ) {
        self.overlay = Some(Overlay::ConfirmRestart(ConfirmRestartOverlay {
            endpoint_id: endpoint_id.clone(),
            label: endpoint_id
                .display_label(&self.config.local_label)
                .to_owned(),
            return_to_navigate: self.mode.is(ClientShellMode::Navigate),
        }));
    }

    pub(in crate::shell) fn open_confirm_close_overlay(
        &mut self,
        workspace_id: shepr_protocol::WorkspaceId,
    ) {
        let Some(snapshot) = self.endpoints.active.snapshot() else {
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
            .filter(|pane| pane.pane_id.workspace_id() == &workspace.workspace_id)
            .count();
        let scope = if pane_count == 1 {
            "1 pane".to_owned()
        } else {
            format!("{pane_count} panes")
        };
        self.overlay = Some(Overlay::ConfirmClose(ConfirmCloseOverlay {
            workspace_id,
            detail: format!("{} - {scope}", workspace.label),
            return_to_navigate: self.mode.is(ClientShellMode::Navigate),
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::confirm_close::ConfirmCloseOverlay;
    use super::context_menu::{ContextMenuOverlay, ContextMenuTarget};
    use super::global_menu::GlobalMenuOverlay;
    use super::help::HelpOverlay;
    use super::navigator::NavigatorOverlay;
    use super::rename::RenameOverlay;
    use super::{Overlay, OverlayScroll, OverlayView};
    use crate::shell::config::ClientShellConfig;
    use crate::shell::state::ClientShellState;
    use crate::shell::view::resolve::overlay_context;
    use crate::tests::test_workspace_id;
    use shepr_config::ClientConfig;

    fn test_state() -> ClientShellState {
        let mut state =
            ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
        state.set_snapshot(Box::new(crate::shell::tests::snapshot()));
        state
    }

    #[test]
    fn layout_returns_none_where_render_gave_up() {
        let state = test_state();
        let ctx = overlay_context(&state);
        let popups = || {
            vec![
                Overlay::Rename(RenameOverlay::workspace(test_workspace_id("w1"), "")),
                Overlay::ConfirmClose(ConfirmCloseOverlay {
                    workspace_id: test_workspace_id("w1"),
                    detail: "detail".to_owned(),
                    return_to_navigate: false,
                }),
                Overlay::ConfirmRestart(super::confirm_restart::ConfirmRestartOverlay {
                    endpoint_id: crate::endpoint::ClientEndpointId::Local,
                    label: "build".to_owned(),
                    return_to_navigate: false,
                }),
                Overlay::Help(HelpOverlay::default()),
                Overlay::Navigator(NavigatorOverlay::default()),
            ]
        };
        for (cols, rows) in [(3, 3), (10, 5)] {
            let screen = ratatui::layout::Rect::new(0, 0, cols, rows);
            for overlay in popups() {
                assert!(
                    overlay.layout(screen, &ctx).is_none(),
                    "{:?} fits {cols}x{rows}",
                    overlay.kind()
                );
            }
        }
        // A menu is clipped to the screen rather than refused while a border still fits.
        let menus = [
            Overlay::ContextMenu(ContextMenuOverlay {
                target: ContextMenuTarget::Workspace {
                    workspace_id: test_workspace_id("w1"),
                },
                x: 0,
                y: 0,
                highlighted: 0,
            }),
            Overlay::GlobalMenu(GlobalMenuOverlay {
                highlighted: 0,
                launcher: ratatui::layout::Rect::new(0, 0, 1, 1),
            }),
        ];
        for overlay in &menus {
            let screen = ratatui::layout::Rect::new(0, 0, 3, 3);
            let (view, scroll) = overlay.layout(screen, &ctx).expect("menu fits");
            assert!(scroll.is_none());
            let rect = match view {
                OverlayView::ContextMenu(menu) | OverlayView::GlobalMenu(menu) => menu.rect,
                _ => panic!("menu view"),
            };
            assert_eq!(rect.intersection(screen), rect);
        }
    }

    #[test]
    fn navigator_layout_keeps_the_selection_in_view() {
        let mut state = test_state();
        let mut projected = crate::shell::tests::snapshot();
        for index in 2..=60 {
            let mut pane = projected.panes[0].clone();
            pane.pane_id = shepr_protocol::PublicPaneId::new(
                &test_workspace_id("w1"),
                shepr_protocol::PanePublicNumber::new(index).expect("nonzero test number"),
            );
            pane.label = Some(format!("agent {index}"));
            projected.panes.push(pane);
        }
        state.set_snapshot(Box::new(projected));
        let ctx = overlay_context(&state);
        let mut overlay = NavigatorOverlay::default();
        let rows = ctx.navigator_index.rows(ctx.active_endpoint_id, &overlay);
        overlay.selected = rows.last().map(|row| row.target.clone());
        let overlay = Overlay::Navigator(overlay);

        let screen = ratatui::layout::Rect::new(0, 0, 106, 24);
        let (view, scroll) = overlay.layout(screen, &ctx).expect("navigator fits");
        let OverlayView::Navigator(view) = view else {
            panic!("navigator view");
        };
        let Some(OverlayScroll::Navigator(start)) = scroll else {
            panic!("navigator scroll");
        };
        assert_eq!(start, view.list.scroll.start());
        assert!(start > 0);
        assert!(view.selected >= start);
        assert!(view.selected < start + view.list.scroll.viewport_rows());
        assert!(view.list.slots.iter().any(|slot| slot.row == view.selected));
    }
}
