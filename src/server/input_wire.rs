use shepr_protocol::{
    ClientKeyCode, ClientKeyKind, ClientMouseButton, ClientMouseKind, ClientMousePosition,
    ClientPaneInputEvent,
};

pub(crate) fn host_modifiers(
    modifiers: shepr_protocol::WireModifiers,
) -> crossterm::event::KeyModifiers {
    crossterm::event::KeyModifiers::from_bits_truncate(modifiers.bits())
}
pub(crate) trait WireKeyKind: Sized {
    fn to_crossterm(self) -> crossterm::event::KeyEventKind;
}

impl WireKeyKind for ClientKeyKind {
    fn to_crossterm(self) -> crossterm::event::KeyEventKind {
        match self {
            Self::Press => crossterm::event::KeyEventKind::Press,
            Self::Repeat => crossterm::event::KeyEventKind::Repeat,
            Self::Release => crossterm::event::KeyEventKind::Release,
        }
    }
}

pub(crate) trait WireKeyCode: Sized {
    fn to_crossterm(&self) -> crossterm::event::KeyCode;
}

impl WireKeyCode for ClientKeyCode {
    fn to_crossterm(&self) -> crossterm::event::KeyCode {
        use crossterm::event::KeyCode;
        match self {
            Self::Backspace => KeyCode::Backspace,
            Self::Enter => KeyCode::Enter,
            Self::Left => KeyCode::Left,
            Self::Right => KeyCode::Right,
            Self::Up => KeyCode::Up,
            Self::Down => KeyCode::Down,
            Self::Home => KeyCode::Home,
            Self::End => KeyCode::End,
            Self::PageUp => KeyCode::PageUp,
            Self::PageDown => KeyCode::PageDown,
            Self::Tab => KeyCode::Tab,
            Self::BackTab => KeyCode::BackTab,
            Self::Delete => KeyCode::Delete,
            Self::Insert => KeyCode::Insert,
            Self::Esc => KeyCode::Esc,
            Self::Char(ch) => KeyCode::Char(*ch),
            Self::F(n) => KeyCode::F(*n),
            Self::Null => KeyCode::Null,
        }
    }
}

pub(crate) trait WireMouseButton: Sized {
    fn to_crossterm(self) -> crossterm::event::MouseButton;
}

impl WireMouseButton for ClientMouseButton {
    fn to_crossterm(self) -> crossterm::event::MouseButton {
        match self {
            Self::Left => crossterm::event::MouseButton::Left,
            Self::Right => crossterm::event::MouseButton::Right,
            Self::Middle => crossterm::event::MouseButton::Middle,
        }
    }
}

pub(crate) trait WireMouseKind: Sized {
    fn to_crossterm(self) -> crossterm::event::MouseEventKind;
}

impl WireMouseKind for ClientMouseKind {
    fn to_crossterm(self) -> crossterm::event::MouseEventKind {
        use crossterm::event::MouseEventKind;
        match self {
            Self::Down(button) => MouseEventKind::Down(button.to_crossterm()),
            Self::Up(button) => MouseEventKind::Up(button.to_crossterm()),
            Self::Drag(button) => MouseEventKind::Drag(button.to_crossterm()),
            Self::Moved => MouseEventKind::Moved,
            Self::ScrollUp => MouseEventKind::ScrollUp,
            Self::ScrollDown => MouseEventKind::ScrollDown,
            Self::ScrollLeft => MouseEventKind::ScrollLeft,
            Self::ScrollRight => MouseEventKind::ScrollRight,
        }
    }
}

pub(crate) trait WirePaneInput: Sized {
    fn text_bytes(&self) -> usize;
    fn to_raw_input_event(&self) -> shepr_termio::input::raw_input::RawInputEvent;
}

impl WirePaneInput for ClientPaneInputEvent {
    /// Text bytes this event delivers to the pane, as charged against
    /// `MAX_INPUT_PAYLOAD`: paste or committed text, or a key's generated text
    /// times its repeat count. Mouse events carry no text.
    fn text_bytes(&self) -> usize {
        match self {
            Self::Key {
                repeat_count,
                generated_text,
                ..
            } => generated_text.as_ref().map_or(0, |text| {
                text.len()
                    .saturating_mul(usize::from((*repeat_count).max(1)))
            }),
            Self::TextCommit(text) | Self::Paste(text) => text.len(),
            Self::Mouse { .. } => 0,
        }
    }

    fn to_raw_input_event(&self) -> shepr_termio::input::raw_input::RawInputEvent {
        match self {
            Self::Key {
                code,
                modifiers,
                kind,
                repeat_count,
                shifted_codepoint,
                generated_text,
            } => {
                let mut key = shepr_termio::input::TerminalKey::new(
                    code.to_crossterm(),
                    host_modifiers(*modifiers),
                )
                .with_kind(kind.to_crossterm())
                .with_repeat_count(*repeat_count)
                .with_generated_text(generated_text.clone());
                if let Some(shifted_codepoint) = shifted_codepoint {
                    key = key.with_shifted_codepoint(*shifted_codepoint);
                }
                shepr_termio::input::raw_input::RawInputEvent::Key(key)
            }
            // Text commits are handled directly by pane input before this conversion.
            Self::TextCommit(_) => shepr_termio::input::raw_input::RawInputEvent::Unsupported,
            Self::Mouse {
                kind,
                position,
                modifiers,
                ..
            } => {
                let (column, row) = match position {
                    ClientMousePosition::Cell { column, row }
                    | ClientMousePosition::Pixels { column, row, .. } => (*column, *row),
                };
                shepr_termio::input::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
                    kind: kind.to_crossterm(),
                    column,
                    row,
                    modifiers: host_modifiers(*modifiers),
                })
            }
            Self::Paste(text) => shepr_termio::input::raw_input::RawInputEvent::Paste(text.clone()),
        }
    }
}
