//! The question a configured machine's Restart asks before it stops anything.

use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use shepr_config::theme::Palette;
use shepr_term::key::TerminalKey;

use super::widgets::{button, panel, panel_inner, popup, row};
use super::{DialogView, OverlayCommand, OverlayEffect, OverlayPaint};
use crate::endpoint::ClientEndpointId;
use crate::limits::{CONFIRM_RESTART_HEIGHT, CONFIRM_RESTART_WIDTH};
use crate::shell::input::hit_test::contains;
use crate::shell::presentation::status::panel_contrast_fg;
use crate::shell::presentation::text::put_text;

#[derive(Debug)]
pub(in crate::shell) struct ConfirmRestartOverlay {
    pub(in crate::shell) endpoint_id: ClientEndpointId,
    /// The machine's name as the sidebar shows it.
    pub(in crate::shell) label: String,
    /// Cancelling returns to Navigate mode only when the question came from it.
    pub(in crate::shell) return_to_navigate: bool,
}

impl ConfirmRestartOverlay {
    /// The question, one line each: what a restart stops and what it restores.
    pub(in crate::shell) fn lines(&self) -> [String; 4] {
        let label = &self.label;
        [
            format!("The shepr server on {label} is a different build."),
            "Restarting stops it, which ends every pane process on that host.".to_owned(),
            "The saved layout is restored with fresh shells,".to_owned(),
            "and agents are resumed where they can be.".to_owned(),
        ]
    }

    pub(super) fn layout(screen: Rect) -> Option<DialogView> {
        let q = popup(screen, CONFIRM_RESTART_WIDTH, CONFIRM_RESTART_HEIGHT)?;
        let i = panel_inner(q)?;
        let rs = row(i, &[13, 12], 2, i.height.saturating_sub(1));
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
        let label = &self.label;
        put_text(
            b,
            i.x,
            i.y,
            i.width,
            &format!(" Restart the server on {label}?"),
            Style::default()
                .fg(p.red)
                .bg(p.panel_bg)
                .add_modifier(Modifier::BOLD),
        );
        for (offset, line) in (1_u16..).zip(self.lines()) {
            put_text(
                b,
                i.x,
                i.y.saturating_add(offset),
                i.width,
                &format!(" {line}"),
                Style::default().fg(p.text).bg(p.panel_bg),
            );
        }
        button(
            b,
            view.primary,
            " ↵ restart ",
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
                OverlayEffect::Command(OverlayCommand::RestartMachine(self.endpoint_id.clone()))
            }
            KeyCode::Esc => OverlayEffect::Command(OverlayCommand::CancelClose {
                return_to_navigate: self.return_to_navigate,
            }),
            _ => OverlayEffect::Unchanged,
        }
    }

    /// A left press anywhere but the restart button closes the question, the popup
    /// included.
    pub(super) fn on_mouse(
        &mut self,
        mouse: MouseEvent,
        view: Option<&DialogView>,
    ) -> OverlayEffect {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return OverlayEffect::Unchanged;
        }
        if view.is_some_and(|view| contains(view.primary, (mouse.column, mouse.row))) {
            OverlayEffect::Command(OverlayCommand::RestartMachine(self.endpoint_id.clone()))
        } else {
            OverlayEffect::Close
        }
    }
}
