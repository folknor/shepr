//! Runtime input policy: what reaches the child's PTY and when (bracketed
//! paste, focus reports, the wheel routing gate). The mode routing and the
//! key and mouse encoders it calls live with the terminal model in
//! `pane/terminal/input.rs`. They are not defined here, next to the policy:
//! the terminal model must not depend on the runtime layer that owns it.

use super::*;

impl PaneRuntime {
    pub fn keyboard_protocol(&self) -> shepr_term::key::KeyboardProtocol {
        // Legacy only when the terminal core is unreadable (a poisoned lock).
        self.terminal
            .keyboard_protocol(shepr_term::key::KeyboardProtocol::legacy())
    }

    pub fn modify_other_keys_level(&self) -> shepr_vt::ModifyOtherKeysLevel {
        self.terminal.modify_other_keys_level()
    }

    /// Samples the current terminal modes for server pane input, with legacy
    /// encoding if the terminal snapshot cannot be read.
    pub fn encode_terminal_key(&self, key: shepr_term::key::TerminalKey) -> Vec<u8> {
        if let Some(modes) = self.read().input_modes() {
            self.encode_terminal_key_with_modes(key, modes)
        } else {
            self.terminal
                .encode_terminal_key(key, shepr_term::key::KeyboardProtocol::legacy())
        }
    }

    pub fn encode_terminal_key_with_modes(
        &self,
        key: shepr_term::key::TerminalKey,
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

    /// Tells the pane it gained or lost terminal focus. The focus is recorded
    /// even when the child has focus reporting off, so a child that turns it
    /// on later while the pane holds focus is told focus-in then.
    ///
    /// The report is queued as a terminal reply, under the reply-order lock a
    /// PTY read holds while it parses and queues its replies. A read that
    /// turns reporting on in a focused pane queues the focus-in it owes; were
    /// this report queued outside that lock, a focus-out recorded after that
    /// parse could reach the child ahead of the read's focus-in and leave the
    /// child believing it holds focus. Under the lock the child hears the
    /// reports in the order the focus was recorded.
    pub fn try_send_focus_event(&self, event: shepr_vt::FocusEvent) {
        write_terminal_response(self.io.as_ref(), || {
            self.terminal
                .note_pane_focus(event)
                .then(|| Bytes::from_static(shepr_vt::encode_focus(event)))
        });
    }

    pub fn wheel_routing(&self) -> Option<WheelRouting> {
        self.terminal.wheel_routing()
    }

    /// Choose input routing from one captured set of terminal modes.
    pub fn wheel_routing_for_modes(modes: shepr_vt::InputModes) -> WheelRouting {
        PaneTerminal::wheel_routing_for_modes(modes)
    }

    pub fn encode_mouse_button_with_modes(
        &self,
        modes: shepr_vt::InputModes,
        kind: crossterm::event::MouseEventKind,
        position: shepr_term::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.terminal
            .encode_mouse_button_with_modes(modes, kind, position, modifiers)
    }

    pub fn encode_mouse_motion_with_modes(
        &self,
        modes: shepr_vt::InputModes,
        kind: crossterm::event::MouseEventKind,
        position: shepr_term::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        self.terminal
            .encode_mouse_motion_with_modes(modes, kind, position, modifiers)
    }

    pub fn encode_mouse_wheel_with_modes(
        &self,
        modes: shepr_vt::InputModes,
        kind: crossterm::event::MouseEventKind,
        position: shepr_term::mouse::Position,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Vec<u8>> {
        if PaneTerminal::wheel_routing_for_modes(modes) != WheelRouting::MouseReport {
            return None;
        }
        self.terminal
            .encode_mouse_wheel_with_modes(modes, kind, position, modifiers)
    }

    pub fn encode_alternate_scroll_with_modes(
        &self,
        modes: shepr_vt::InputModes,
        kind: crossterm::event::MouseEventKind,
    ) -> Option<Vec<u8>> {
        if PaneTerminal::wheel_routing_for_modes(modes) != WheelRouting::AlternateScroll {
            return None;
        }
        let key = match kind {
            crossterm::event::MouseEventKind::ScrollUp => crossterm::event::KeyCode::Up,
            crossterm::event::MouseEventKind::ScrollDown => crossterm::event::KeyCode::Down,
            _ => return None,
        };
        Some(self.encode_terminal_key_with_modes(
            shepr_term::key::TerminalKey::new(key, crossterm::event::KeyModifiers::empty()),
            modes,
        ))
    }
}
