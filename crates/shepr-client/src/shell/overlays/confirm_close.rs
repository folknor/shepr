//! The close-workspace confirmation.

use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use shepr_config::theme::Palette;
use shepr_term::key::TerminalKey;

use super::widgets::{button, panel, panel_inner, popup, row};
use super::{DialogView, OverlayCommand, OverlayEffect, OverlayPaint};
use crate::shell::input::hit_test::contains;
use crate::shell::presentation::status::panel_contrast_fg;
use crate::shell::presentation::text::put_text;

const TITLE: &str = "Close workspace?";

#[derive(Debug)]
pub(in crate::shell) struct ConfirmCloseOverlay {
    pub(in crate::shell) workspace_id: shepr_protocol::WorkspaceId,
    pub(in crate::shell) detail: String,
    /// Cancelling returns to Navigate mode only when the dialog came from it;
    /// otherwise the user lands back in the mode they were in.
    pub(in crate::shell) return_to_navigate: bool,
}

impl ConfirmCloseOverlay {
    pub(super) fn layout(screen: Rect) -> Option<DialogView> {
        let q = popup(screen, 64, 6)?;
        let i = panel_inner(q)?;
        let rs = row(i, &[13, 12], 2, 3);
        let [ok, cancel] = rs.as_slice() else {
            return None;
        };
        Some(DialogView {
            popup: q,
            inner: i,
            input: None,
            primary: *ok,
            clear: None,
            cancel: *cancel,
        })
    }

    pub(super) fn draw(&self, b: &mut Buffer, view: &DialogView, p: &Palette) -> OverlayPaint {
        panel(b, view.popup, p.red, p.panel_bg);
        let i = view.inner;
        put_text(
            b,
            i.x,
            i.y,
            i.width,
            &format!(" {TITLE}"),
            Style::default()
                .fg(p.red)
                .bg(p.panel_bg)
                .add_modifier(Modifier::BOLD),
        );
        put_text(
            b,
            i.x,
            i.y + 1,
            i.width,
            &format!(" {}", self.detail),
            Style::default().fg(p.text).bg(p.panel_bg),
        );
        button(
            b,
            view.primary,
            " ↵ confirm ",
            Style::default()
                .fg(panel_contrast_fg(p))
                .bg(p.red)
                .add_modifier(Modifier::BOLD),
        );
        button(
            b,
            view.cancel,
            " esc cancel ",
            Style::default()
                .fg(p.text)
                .bg(p.surface0)
                .add_modifier(Modifier::BOLD),
        );
        OverlayPaint {
            opaque: vec![view.popup],
            backdrop: true,
            cursor: None,
        }
    }

    pub(super) fn on_key(&mut self, key: &TerminalKey) -> OverlayEffect {
        match key.code {
            KeyCode::Enter => {
                OverlayEffect::Command(OverlayCommand::CloseWorkspace(self.workspace_id))
            }
            KeyCode::Esc => OverlayEffect::Command(OverlayCommand::CancelClose {
                return_to_navigate: self.return_to_navigate,
            }),
            _ => OverlayEffect::Unchanged,
        }
    }

    /// A left press anywhere but the confirm button closes the dialog, the popup included.
    pub(super) fn on_mouse(
        &mut self,
        mouse: MouseEvent,
        view: Option<&DialogView>,
    ) -> OverlayEffect {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return OverlayEffect::Unchanged;
        }
        if view.is_some_and(|view| contains(view.primary, (mouse.column, mouse.row))) {
            OverlayEffect::Command(OverlayCommand::CloseWorkspace(self.workspace_id))
        } else {
            OverlayEffect::Close
        }
    }
}
