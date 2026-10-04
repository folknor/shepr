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
        key: shepr_term::key::TerminalKey,
        protocol: shepr_term::key::KeyboardProtocol,
    ) -> Vec<u8> {
        self.encode_terminal_key_with_input_modes(key, protocol, None)
    }

    pub(crate) fn encode_terminal_key_with_modes(
        &self,
        key: shepr_term::key::TerminalKey,
        modes: shepr_vt::InputModes,
    ) -> Vec<u8> {
        let protocol = shepr_term::key::KeyboardProtocol::from_flags(modes.kitty_keyboard_flags());
        self.encode_terminal_key_with_input_modes(key, protocol, Some(modes))
    }

    fn encode_terminal_key_with_input_modes(
        &self,
        key: shepr_term::key::TerminalKey,
        protocol: shepr_term::key::KeyboardProtocol,
        input_modes: Option<shepr_vt::InputModes>,
    ) -> Vec<u8> {
        // Character keys follow the caller's protocol; every other key follows
        // the modes the child negotiated with this pane.
        if matches!(key.code, crossterm::event::KeyCode::Char(_)) {
            return shepr_term::key::encode_terminal_key(key, protocol);
        }
        let modes = input_modes
            .map(|modes| shepr_term::key::KeyEncodeModes {
                kitty_flags: modes.kitty_keyboard_flags(),
                modify_other_keys: modes.modify_other_keys_level(),
                application_cursor: modes.application_cursor_keys_enabled(),
            })
            .or_else(|| {
                self.core
                    .lock()
                    .ok()
                    .map(|core| shepr_term::key::KeyEncodeModes {
                        kitty_flags: core.terminal.kitty_keyboard_flags(),
                        modify_other_keys: core.terminal.modify_other_keys_level(),
                        application_cursor: core
                            .terminal
                            .mode_get(shepr_vt::DecMode::ApplicationCursorKeys),
                    })
            });
        let Some(modes) = modes else {
            return shepr_term::key::encode_terminal_key(key, protocol);
        };
        shepr_term::key::encode_terminal_key_with_modes(key, modes)
    }

    pub(crate) fn encode_mouse_button_with_modes(
        &self,
        modes: shepr_vt::InputModes,
        kind: crossterm::event::MouseEventKind,
        position: shepr_term::mouse::Position,
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
        position: shepr_term::mouse::Position,
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
        position: shepr_term::mouse::Position,
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
        position: shepr_term::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        shepr_term::mouse::encode_pane_mouse_report(
            kind,
            position,
            modifiers,
            modes.mouse_protocol()?,
            modes.pixel_mouse(),
        )
    }
}
