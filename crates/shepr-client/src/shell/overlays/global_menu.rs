//! The global menu opened from the sidebar's launcher: its items, layout and input, and the
//! shell work its actions do.

use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use shepr_term::key::TerminalKey;

use super::{MenuView, Overlay, OverlayCommand, OverlayContext, OverlayEffect, OverlayPaint};
use super::{draw_menu, menu_view};
use crate::shell::input::hit_test::contains;
use crate::shell::presentation::text::display_width;
use crate::shell::state::{ClientShellInput, ClientShellState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::shell) enum GlobalMenuAction {
    Binding(shepr_termio::input::KeybindAction),
}

fn global_menu_items() -> Vec<(&'static str, GlobalMenuAction)> {
    vec![
        (
            "keybinds",
            GlobalMenuAction::Binding(shepr_termio::input::KeybindAction::Help),
        ),
        (
            "detach",
            GlobalMenuAction::Binding(shepr_termio::input::KeybindAction::Detach),
        ),
    ]
}

#[derive(Debug)]
pub(in crate::shell) struct GlobalMenuOverlay {
    pub(super) highlighted: usize,
    /// The sidebar's launcher as drawn when the menu opened; the menu sits above it.
    pub(super) launcher: Rect,
}

impl GlobalMenuOverlay {
    pub(super) fn layout(&self, screen: Rect) -> Option<MenuView> {
        let items = global_menu_items();
        let width = items
            .iter()
            .map(|(label, _)| display_width(label))
            .max()
            .unwrap_or(8)
            .saturating_add(4)
            .min(screen.width.max(1));
        let height = u16::try_from(items.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .min(screen.height.max(1));
        let launcher = self.launcher;
        let x = launcher
            .right()
            .saturating_sub(width)
            .min(screen.right().saturating_sub(width));
        let y = launcher.y.saturating_sub(height).max(screen.y);
        menu_view(Rect::new(x, y, width, height), items.len())
    }

    pub(super) fn draw(
        &self,
        buffer: &mut Buffer,
        view: &MenuView,
        ctx: &OverlayContext<'_>,
    ) -> OverlayPaint {
        let items = global_menu_items();
        draw_menu(
            buffer,
            view,
            self.highlighted,
            |index| items.get(index).map(|(label, _)| format!(" {label}")),
            |_| true,
            ctx.palette,
        )
    }

    fn move_selection(&mut self, delta: isize) {
        let max_index = global_menu_items().len().saturating_sub(1);
        self.highlighted = self
            .highlighted
            .checked_add_signed(delta)
            .unwrap_or(0)
            .min(max_index);
    }

    /// Activating an index with no item does nothing and leaves the menu open.
    fn activate(&self, index: usize) -> OverlayEffect {
        match global_menu_items().get(index) {
            Some((_, action)) => OverlayEffect::Command(OverlayCommand::GlobalMenu(*action)),
            None => OverlayEffect::Unchanged,
        }
    }

    pub(super) fn on_key(&mut self, key: &TerminalKey) -> OverlayEffect {
        match key.code {
            KeyCode::Esc => OverlayEffect::Close,
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                OverlayEffect::Changed
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                OverlayEffect::Changed
            }
            KeyCode::Enter => self.activate(self.highlighted),
            _ => OverlayEffect::Unchanged,
        }
    }

    /// `launcher` is the sidebar's menu launcher on screen now: pressing it again closes the
    /// menu.
    pub(super) fn on_mouse(
        &mut self,
        mouse: MouseEvent,
        view: Option<&MenuView>,
        launcher: Option<Rect>,
    ) -> OverlayEffect {
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
            MouseEventKind::Down(MouseButton::Left) => {
                if launcher.is_some_and(|launcher| contains(launcher, point)) {
                    OverlayEffect::Command(OverlayCommand::ToggleGlobalMenu)
                } else if let Some(index) = row_hit {
                    self.activate(index)
                } else {
                    OverlayEffect::Close
                }
            }
            _ => OverlayEffect::Unchanged,
        }
    }
}

impl ClientShellState {
    pub(in crate::shell) fn toggle_global_menu(&mut self) {
        if matches!(self.overlay, Some(Overlay::GlobalMenu(_))) {
            self.overlay = None;
        } else {
            self.overlay = Some(Overlay::GlobalMenu(GlobalMenuOverlay {
                highlighted: 0,
                launcher: self.presentation.shown().global_launcher(),
            }));
        }
    }

    /// Does what a chosen menu item says. The menu is already closed.
    pub(super) fn activate_global_menu_action(
        &mut self,
        action: GlobalMenuAction,
        outcome: &mut ClientShellInput,
    ) {
        match action {
            GlobalMenuAction::Binding(binding) => {
                self.record_binding(&binding, outcome);
            }
        }
        outcome.repaint = true;
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;

    use super::{GlobalMenuOverlay, OverlayCommand, OverlayEffect};
    use crate::shell::overlays::MenuView;

    fn press(column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn launcher_click_toggles() {
        let mut menu = GlobalMenuOverlay {
            highlighted: 0,
            launcher: Rect::new(20, 20, 4, 1),
        };
        let view = MenuView {
            rect: Rect::new(18, 16, 10, 4),
            rows: vec![(Rect::new(19, 17, 8, 1), 0), (Rect::new(19, 18, 8, 1), 1)],
        };
        let launcher = Some(Rect::new(20, 20, 4, 1));

        // The launcher closes the menu it opened, through the shell's toggle.
        let effect = menu.on_mouse(press(21, 20), Some(&view), launcher);
        assert!(matches!(
            effect,
            OverlayEffect::Command(OverlayCommand::ToggleGlobalMenu)
        ));
        // A row activates its item.
        let effect = menu.on_mouse(press(20, 18), Some(&view), launcher);
        assert!(matches!(
            effect,
            OverlayEffect::Command(OverlayCommand::GlobalMenu(_))
        ));
        // Anywhere else closes, and so does a launcher the sidebar no longer draws.
        let effect = menu.on_mouse(press(0, 0), Some(&view), launcher);
        assert!(matches!(effect, OverlayEffect::Close));
        let effect = menu.on_mouse(press(21, 20), Some(&view), None);
        assert!(matches!(effect, OverlayEffect::Close));
    }
}
