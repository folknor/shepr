//! Mode routing and protocol encoding for input to the pane's child: how a
//! wheel event is routed and how keys and mouse events are encoded under the
//! modes the child negotiated. These read only the terminal model, so they
//! live with it; the runtime's input policy (`pane/runtime/input.rs`) calls
//! them and adds the gates that need a live PTY. Keeping them here keeps the
//! terminal model free of any dependency on the runtime that owns it.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelRouting {
    HostScroll,
    MouseReport,
    AlternateScroll,
}

impl PaneTerminal {
    pub(crate) fn wheel_routing(&self) -> Option<WheelRouting> {
        self.input_modes().map(Self::wheel_routing_for_modes)
    }

    pub(crate) fn wheel_routing_for_modes(modes: shepr_vt::InputModes) -> WheelRouting {
        if modes.mouse_tracking_enabled() {
            WheelRouting::MouseReport
        } else if modes.alternate_screen_active() && modes.mouse_alternate_scroll_enabled() {
            WheelRouting::AlternateScroll
        } else {
            WheelRouting::HostScroll
        }
    }

    pub(crate) fn encode_terminal_key(
        &self,
        key: shepr_termio::input::TerminalKey,
        protocol: shepr_termio::input::KeyboardProtocol,
    ) -> Vec<u8> {
        self.encode_terminal_key_with_input_modes(key, protocol, None)
    }

    pub(crate) fn encode_terminal_key_with_modes(
        &self,
        key: shepr_termio::input::TerminalKey,
        modes: shepr_vt::InputModes,
    ) -> Vec<u8> {
        let protocol =
            shepr_termio::input::KeyboardProtocol::from_flags(modes.kitty_keyboard_flags());
        self.encode_terminal_key_with_input_modes(key, protocol, Some(modes))
    }

    fn encode_terminal_key_with_input_modes(
        &self,
        key: shepr_termio::input::TerminalKey,
        protocol: shepr_termio::input::KeyboardProtocol,
        input_modes: Option<shepr_vt::InputModes>,
    ) -> Vec<u8> {
        let repeat_count = key.repeat_count;
        let first = key.with_repeat_count(1);
        let mut bytes =
            self.encode_terminal_key_once_with_modes(first.clone(), protocol, input_modes);
        if repeat_count > 1 && first.kind != crossterm::event::KeyEventKind::Release {
            let repeated = first.with_kind(crossterm::event::KeyEventKind::Repeat);
            let repeated_bytes =
                self.encode_terminal_key_once_with_modes(repeated, protocol, input_modes);
            for _ in 1..repeat_count {
                bytes.extend_from_slice(&repeated_bytes);
            }
        }
        bytes
    }

    pub(super) fn encode_terminal_key_once_with_modes(
        &self,
        key: shepr_termio::input::TerminalKey,
        protocol: shepr_termio::input::KeyboardProtocol,
        input_modes: Option<shepr_vt::InputModes>,
    ) -> Vec<u8> {
        // Character keys follow the caller's protocol; every other key follows
        // the modes the child negotiated with this pane.
        if matches!(key.code, crossterm::event::KeyCode::Char(_)) {
            return shepr_termio::input::encode_terminal_key(key, protocol);
        }
        let modes = input_modes
            .map(|modes| shepr_termio::input::KeyEncodeModes {
                kitty_flags: modes.kitty_keyboard_flags(),
                modify_other_keys: modes.modify_other_keys_level(),
                application_cursor: modes.application_cursor_keys_enabled(),
            })
            .or_else(|| {
                shepr_vt::lock_terminal_core(&self.core).ok().map(|core| {
                    shepr_termio::input::KeyEncodeModes {
                        kitty_flags: core.terminal.kitty_keyboard_flags(),
                        modify_other_keys: core.terminal.modify_other_keys_level(),
                        application_cursor: core
                            .terminal
                            .mode_get(shepr_vt::DecMode::ApplicationCursorKeys),
                    }
                })
            });
        let Some(modes) = modes else {
            return shepr_termio::input::encode_terminal_key(key, protocol);
        };
        shepr_termio::input::encode_terminal_key_with_modes(key, modes)
    }

    pub(crate) fn encode_mouse_button_with_modes(
        &self,
        modes: shepr_vt::InputModes,
        kind: crossterm::event::MouseEventKind,
        position: shepr_termio::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        use crossterm::event::MouseEventKind;
        if !matches!(
            kind,
            MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_)
        ) {
            return None;
        }
        self.encode_mouse_event_with_modes(modes, kind, position, modifiers)
    }

    pub(crate) fn encode_mouse_motion_with_modes(
        &self,
        modes: shepr_vt::InputModes,
        kind: crossterm::event::MouseEventKind,
        position: shepr_termio::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        if kind != crossterm::event::MouseEventKind::Moved {
            return None;
        }
        self.encode_mouse_event_with_modes(modes, kind, position, modifiers)
    }

    pub(crate) fn encode_mouse_wheel_with_modes(
        &self,
        modes: shepr_vt::InputModes,
        kind: crossterm::event::MouseEventKind,
        position: shepr_termio::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        use crossterm::event::MouseEventKind;
        if !matches!(
            kind,
            MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ) {
            return None;
        }
        self.encode_mouse_event_with_modes(modes, kind, position, modifiers)
    }

    fn encode_mouse_event_with_modes(
        &self,
        modes: shepr_vt::InputModes,
        kind: crossterm::event::MouseEventKind,
        position: shepr_termio::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        let core = shepr_vt::lock_terminal_core(&self.core).ok()?;
        let terminal = &core.terminal;
        let protocol = modes.mouse_protocol()?;
        let cell_encoding = match protocol.encoding {
            shepr_vt::MouseEncoding::Default => shepr_termio::input::MouseProtocolEncoding::Default,
            shepr_vt::MouseEncoding::Utf8 => shepr_termio::input::MouseProtocolEncoding::Utf8,
            shepr_vt::MouseEncoding::Sgr => shepr_termio::input::MouseProtocolEncoding::Sgr,
        };
        // Reports are 1-based. Pixel positions already arrive 1-based; cell
        // positions are shifted here. Under SGR-pixels (mode 1016) a cell
        // position is mapped to the top-left pixel of that cell using the same
        // integer cell pitch the pixel fallback below uses, so the child maps
        // it straight back to the cell. Only when the pane has no pixel
        // geometry at all is the cell sent as-is in SGR form: the child can't
        // know a cell size either then, and a report beats a dropped click.
        let cell_pitch = || {
            let cols = u32::from(terminal.cols());
            let rows = u32::from(terminal.rows());
            let width_px = terminal.width_px();
            let height_px = terminal.height_px();
            (cols > 0 && rows > 0 && width_px > 0 && height_px > 0)
                .then(|| ((width_px / cols).max(1), (height_px / rows).max(1)))
        };
        let (encoding, x, y) = match position {
            shepr_termio::input::mouse::Position::Cell { column, row }
                if protocol.pixels_requested =>
            {
                match cell_pitch() {
                    Some((cell_width, cell_height)) => (
                        shepr_termio::input::MouseProtocolEncoding::SgrPixels,
                        u32::from(column)
                            .saturating_mul(cell_width)
                            .saturating_add(1),
                        u32::from(row).saturating_mul(cell_height).saturating_add(1),
                    ),
                    None => (
                        shepr_termio::input::MouseProtocolEncoding::Sgr,
                        u32::from(column) + 1,
                        u32::from(row) + 1,
                    ),
                }
            }
            shepr_termio::input::mouse::Position::Cell { column, row } => {
                (cell_encoding, u32::from(column) + 1, u32::from(row) + 1)
            }
            shepr_termio::input::mouse::Position::Pixels { x, y } if protocol.pixels_requested => {
                (shepr_termio::input::MouseProtocolEncoding::SgrPixels, x, y)
            }
            shepr_termio::input::mouse::Position::Pixels { x, y } => {
                let cols = u32::from(terminal.cols());
                let rows = u32::from(terminal.rows());
                let (cell_width, cell_height) = cell_pitch()?;
                (
                    cell_encoding,
                    (x.saturating_sub(1) / cell_width).min(cols - 1) + 1,
                    (y.saturating_sub(1) / cell_height).min(rows - 1) + 1,
                )
            }
        };
        shepr_termio::input::encode_mouse_event(kind, x, y, modifiers, protocol.mode, encoding)
    }
}
