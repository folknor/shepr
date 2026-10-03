//! Runtime input policy: what reaches the child's PTY and when (bracketed
//! paste, focus reports, the wheel routing gate). The mode routing and the
//! key and mouse encoders it calls live with the terminal model in
//! `pane/terminal/input.rs`. They are not defined here, next to the policy:
//! the terminal model must not depend on the runtime layer that owns it.

use super::*;

impl PaneRuntime {
    pub fn keyboard_protocol(&self) -> shepr_termio::input::KeyboardProtocol {
        // Legacy only when the terminal core is unreadable (a poisoned lock).
        self.terminal
            .keyboard_protocol(shepr_termio::input::KeyboardProtocol::legacy())
    }

    pub fn modify_other_keys_level(&self) -> shepr_vt::ModifyOtherKeysLevel {
        self.terminal.modify_other_keys_level()
    }

    pub fn encode_terminal_key(&self, key: shepr_termio::input::TerminalKey) -> Vec<u8> {
        if let Some(modes) = self.read().input_modes() {
            self.encode_terminal_key_with_modes(key, modes)
        } else {
            self.terminal
                .encode_terminal_key(key, shepr_termio::input::KeyboardProtocol::legacy())
        }
    }

    pub fn encode_terminal_key_with_modes(
        &self,
        key: shepr_termio::input::TerminalKey,
        modes: shepr_vt::InputModes,
    ) -> Vec<u8> {
        self.terminal.encode_terminal_key_with_modes(key, modes)
    }

    pub fn try_send_bytes(&self, bytes: Bytes) -> Result<(), shepr_pty::ChildIoSendError> {
        self.io.try_write_user_input(bytes)
    }

    pub fn try_send_paste(&self, text: String) -> Result<(), shepr_pty::ChildIoSendError> {
        self.try_send_bytes(self.paste_payload(text))
    }

    pub(super) fn paste_payload(&self, text: String) -> Bytes {
        let bracketed = self.read().bracketed_paste_enabled();
        let payload = if bracketed {
            // Clipboard controls must not change how a child interprets the
            // bracketed wrapper. Preserve ordinary pasted whitespace only.
            let safe: String = text
                .replace("\x1b[201~", "")
                .replace("\x1b[200~", "")
                .chars()
                .filter(|ch| !ch.is_control() || matches!(*ch, '\t' | '\r' | '\n'))
                .collect();
            format!("\x1b[200~{safe}\x1b[201~")
        } else {
            text
        };
        Bytes::from(payload)
    }

    pub fn try_send_focus_event(&self, event: shepr_vt::FocusEvent) {
        if !self.read().focus_reporting_enabled() {
            return;
        }

        let bytes = shepr_vt::encode_focus(event);
        if let Err(err) = self.try_send_bytes(Bytes::from_static(bytes)) {
            warn!(error = %err, ?event, "failed to forward pane focus event");
        }
    }

    pub fn wheel_routing(&self) -> Option<WheelRouting> {
        self.terminal.wheel_routing()
    }

    pub fn wheel_routing_for_modes(&self, modes: shepr_vt::InputModes) -> WheelRouting {
        PaneTerminal::wheel_routing_for_modes(modes)
    }

    pub fn encode_mouse_button(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: shepr_termio::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.encode_mouse_button_with_modes(self.read().input_modes()?, kind, position, modifiers)
    }

    pub fn encode_mouse_button_with_modes(
        &self,
        modes: shepr_vt::InputModes,
        kind: crossterm::event::MouseEventKind,
        position: shepr_termio::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.terminal
            .encode_mouse_button_with_modes(modes, kind, position, modifiers)
    }

    pub fn encode_mouse_motion(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: shepr_termio::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.encode_mouse_motion_with_modes(self.read().input_modes()?, kind, position, modifiers)
    }

    pub fn encode_mouse_motion_with_modes(
        &self,
        modes: shepr_vt::InputModes,
        kind: crossterm::event::MouseEventKind,
        position: shepr_termio::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.terminal
            .encode_mouse_motion_with_modes(modes, kind, position, modifiers)
    }

    pub fn encode_mouse_wheel(
        &self,
        kind: crossterm::event::MouseEventKind,
        position: shepr_termio::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.encode_mouse_wheel_with_modes(self.read().input_modes()?, kind, position, modifiers)
    }

    pub fn encode_mouse_wheel_with_modes(
        &self,
        modes: shepr_vt::InputModes,
        kind: crossterm::event::MouseEventKind,
        position: shepr_termio::input::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        if self.wheel_routing_for_modes(modes) != WheelRouting::MouseReport {
            return None;
        }
        self.terminal
            .encode_mouse_wheel_with_modes(modes, kind, position, modifiers)
    }

    pub fn pixel_size(&self) -> Option<(u32, u32)> {
        self.current_size
            .text_area_px()
            .map(|(width, height)| (u32::from(width), u32::from(height)))
    }

    pub fn encode_alternate_scroll(
        &self,
        kind: crossterm::event::MouseEventKind,
    ) -> Option<Vec<u8>> {
        self.encode_alternate_scroll_with_modes(self.read().input_modes()?, kind)
    }

    pub fn encode_alternate_scroll_with_modes(
        &self,
        modes: shepr_vt::InputModes,
        kind: crossterm::event::MouseEventKind,
    ) -> Option<Vec<u8>> {
        if self.wheel_routing_for_modes(modes) != WheelRouting::AlternateScroll {
            return None;
        }
        let key = match kind {
            crossterm::event::MouseEventKind::ScrollUp => crossterm::event::KeyCode::Up,
            crossterm::event::MouseEventKind::ScrollDown => crossterm::event::KeyCode::Down,
            _ => return None,
        };
        Some(self.encode_terminal_key_with_modes(
            shepr_termio::input::TerminalKey::new(key, crossterm::event::KeyModifiers::empty()),
            modes,
        ))
    }
}
