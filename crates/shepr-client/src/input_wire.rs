//! Conversions between wire input and the host or pane input models.

pub(crate) fn wire_modifiers(
    modifiers: crossterm::event::KeyModifiers,
) -> shepr_protocol::WireModifiers {
    shepr_protocol::WireModifiers::from_bits_retain(modifiers.bits())
}

use shepr_protocol::{
    ClientKeyCode, ClientKeyKind, ClientMouseButton, ClientMouseKind, ClientPaneInputEvent,
};

pub(crate) trait WireKeyKind: Sized {
    fn from_crossterm(kind: crossterm::event::KeyEventKind) -> Self;
}

impl WireKeyKind for ClientKeyKind {
    fn from_crossterm(kind: crossterm::event::KeyEventKind) -> Self {
        match kind {
            crossterm::event::KeyEventKind::Press => Self::Press,
            crossterm::event::KeyEventKind::Repeat => Self::Repeat,
            crossterm::event::KeyEventKind::Release => Self::Release,
        }
    }
}

pub(crate) trait WireKeyCode: Sized {
    fn from_crossterm(code: crossterm::event::KeyCode) -> Option<Self>;
}

impl WireKeyCode for ClientKeyCode {
    fn from_crossterm(code: crossterm::event::KeyCode) -> Option<Self> {
        use crossterm::event::KeyCode;
        Some(match code {
            KeyCode::Backspace => Self::Backspace,
            KeyCode::Enter => Self::Enter,
            KeyCode::Left => Self::Left,
            KeyCode::Right => Self::Right,
            KeyCode::Up => Self::Up,
            KeyCode::Down => Self::Down,
            KeyCode::Home => Self::Home,
            KeyCode::End => Self::End,
            KeyCode::PageUp => Self::PageUp,
            KeyCode::PageDown => Self::PageDown,
            KeyCode::Tab => Self::Tab,
            KeyCode::BackTab => Self::BackTab,
            KeyCode::Delete => Self::Delete,
            KeyCode::Insert => Self::Insert,
            KeyCode::Esc => Self::Esc,
            KeyCode::Char(ch) => Self::Char(ch),
            KeyCode::F(n) => Self::F(n),
            KeyCode::Null => Self::Null,
            _ => return None,
        })
    }
}

pub(crate) trait WireMouseButton: Sized {
    fn from_crossterm(button: crossterm::event::MouseButton) -> Self;
}

impl WireMouseButton for ClientMouseButton {
    fn from_crossterm(button: crossterm::event::MouseButton) -> Self {
        match button {
            crossterm::event::MouseButton::Left => Self::Left,
            crossterm::event::MouseButton::Right => Self::Right,
            crossterm::event::MouseButton::Middle => Self::Middle,
        }
    }
}

pub(crate) trait WireMouseKind: Sized {
    fn from_crossterm(kind: crossterm::event::MouseEventKind) -> Option<Self>;
}

impl WireMouseKind for ClientMouseKind {
    fn from_crossterm(kind: crossterm::event::MouseEventKind) -> Option<Self> {
        use crossterm::event::MouseEventKind;
        Some(match kind {
            MouseEventKind::Down(button) => Self::Down(ClientMouseButton::from_crossterm(button)),
            MouseEventKind::Up(button) => Self::Up(ClientMouseButton::from_crossterm(button)),
            MouseEventKind::Drag(button) => Self::Drag(ClientMouseButton::from_crossterm(button)),
            MouseEventKind::Moved => Self::Moved,
            MouseEventKind::ScrollUp => Self::ScrollUp,
            MouseEventKind::ScrollDown => Self::ScrollDown,
            MouseEventKind::ScrollLeft => Self::ScrollLeft,
            MouseEventKind::ScrollRight => Self::ScrollRight,
        })
    }
}

pub(crate) trait WirePaneInput: Sized {
    fn text_bytes(&self) -> usize;
    fn from_terminal_key(key: shepr_termio::input::TerminalKey) -> Option<Self>;
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

    fn from_terminal_key(key: shepr_termio::input::TerminalKey) -> Option<Self> {
        Some(Self::Key {
            code: ClientKeyCode::from_crossterm(key.code)?,
            modifiers: wire_modifiers(key.modifiers),
            kind: ClientKeyKind::from_crossterm(key.kind),
            repeat_count: key.repeat_count,
            shifted_codepoint: key.shifted_codepoint,
            generated_text: key.generated_text,
        })
    }
}
