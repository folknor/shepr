use super::*;
use serde::{Deserialize, Serialize};
use shepr_core::limits::PALETTE_COLOR_COUNT;
pub use shepr_vt::KittyKeyboardFlags;

// ---------------------------------------------------------------------------
// Client → Server messages
// ---------------------------------------------------------------------------

/// Size of the pane surface requested by a client-owned shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientSurfaceSize {
    pub cols: u16,
    pub rows: u16,
}

impl ClientSurfaceSize {
    /// Fit a requested grid into the server's surface limits. Keep its width
    /// first so the shell layout tracks the host; trim excess height.
    pub fn clamped(self) -> Self {
        let cols = self
            .cols
            .clamp(MIN_SURFACE_DIMENSION, MAX_SURFACE_DIMENSION);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "bounded by MAX_SURFACE_DIMENSION (a u16) via .min(...), so this never truncates"
        )]
        let max_rows =
            (MAX_SURFACE_CELLS / usize::from(cols)).min(usize::from(MAX_SURFACE_DIMENSION)) as u16;
        Self {
            cols,
            rows: self.rows.clamp(MIN_SURFACE_DIMENSION, max_rows),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientKeyKind {
    Press,
    Repeat,
    Release,
}

impl ClientKeyKind {
    /// Converts this wire key kind to the host terminal event kind.
    pub fn to_host(self) -> ratatui::crossterm::event::KeyEventKind {
        match self {
            Self::Press => ratatui::crossterm::event::KeyEventKind::Press,
            Self::Repeat => ratatui::crossterm::event::KeyEventKind::Repeat,
            Self::Release => ratatui::crossterm::event::KeyEventKind::Release,
        }
    }

    /// Converts a host terminal event kind to its wire representation.
    pub fn from_host(kind: ratatui::crossterm::event::KeyEventKind) -> Self {
        match kind {
            ratatui::crossterm::event::KeyEventKind::Press => Self::Press,
            ratatui::crossterm::event::KeyEventKind::Repeat => Self::Repeat,
            ratatui::crossterm::event::KeyEventKind::Release => Self::Release,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[expect(
    variant_size_differences,
    reason = "a char is four bytes; the whole key code is a small value"
)]
pub enum ClientKeyCode {
    Backspace,
    Enter,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    BackTab,
    Delete,
    Insert,
    Esc,
    Char(char),
    F(u8),
    Null,
}

impl ClientKeyCode {
    /// Converts this wire key code to the host terminal key code.
    pub fn to_host(&self) -> ratatui::crossterm::event::KeyCode {
        use ratatui::crossterm::event::KeyCode;
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

    /// Converts a host terminal key code when the wire model represents it.
    pub fn from_host(code: ratatui::crossterm::event::KeyCode) -> Option<Self> {
        use ratatui::crossterm::event::KeyCode;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClientMouseButton {
    Left,
    Right,
    Middle,
}

impl ClientMouseButton {
    /// Converts this wire mouse button to the host terminal button.
    pub fn to_host(self) -> ratatui::crossterm::event::MouseButton {
        match self {
            Self::Left => ratatui::crossterm::event::MouseButton::Left,
            Self::Right => ratatui::crossterm::event::MouseButton::Right,
            Self::Middle => ratatui::crossterm::event::MouseButton::Middle,
        }
    }

    /// Converts a host terminal mouse button to its wire representation.
    pub fn from_host(button: ratatui::crossterm::event::MouseButton) -> Self {
        match button {
            ratatui::crossterm::event::MouseButton::Left => Self::Left,
            ratatui::crossterm::event::MouseButton::Right => Self::Right,
            ratatui::crossterm::event::MouseButton::Middle => Self::Middle,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMouseKind {
    Down(ClientMouseButton),
    Up(ClientMouseButton),
    Drag(ClientMouseButton),
    Moved,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

impl ClientMouseKind {
    /// Converts this wire mouse kind to the host terminal event kind.
    pub fn to_host(self) -> ratatui::crossterm::event::MouseEventKind {
        use ratatui::crossterm::event::MouseEventKind;
        match self {
            Self::Down(button) => MouseEventKind::Down(button.to_host()),
            Self::Up(button) => MouseEventKind::Up(button.to_host()),
            Self::Drag(button) => MouseEventKind::Drag(button.to_host()),
            Self::Moved => MouseEventKind::Moved,
            Self::ScrollUp => MouseEventKind::ScrollUp,
            Self::ScrollDown => MouseEventKind::ScrollDown,
            Self::ScrollLeft => MouseEventKind::ScrollLeft,
            Self::ScrollRight => MouseEventKind::ScrollRight,
        }
    }

    /// Converts a host terminal mouse kind to its wire representation.
    pub fn from_host(kind: ratatui::crossterm::event::MouseEventKind) -> Self {
        use ratatui::crossterm::event::MouseEventKind;
        match kind {
            MouseEventKind::Down(button) => Self::Down(ClientMouseButton::from_host(button)),
            MouseEventKind::Up(button) => Self::Up(ClientMouseButton::from_host(button)),
            MouseEventKind::Drag(button) => Self::Drag(ClientMouseButton::from_host(button)),
            MouseEventKind::Moved => Self::Moved,
            MouseEventKind::ScrollUp => Self::ScrollUp,
            MouseEventKind::ScrollDown => Self::ScrollDown,
            MouseEventKind::ScrollLeft => Self::ScrollLeft,
            MouseEventKind::ScrollRight => Self::ScrollRight,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[expect(
    variant_size_differences,
    reason = "a Copy value of at most a dozen bytes; boxing the pixel form would allocate per mouse event"
)]
pub enum ClientMousePosition {
    Cell {
        column: u16,
        row: u16,
    },
    Pixels {
        x: u32,
        y: u32,
        column: u16,
        row: u16,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientMouseGeometry {
    pub cols: u16,
    pub rows: u16,
    pub width_px: u32,
    pub height_px: u32,
}

/// Modifier bits carried by semantic input messages.
/// The host input edge translates these bits to and from crossterm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WireModifiers(u8);

impl WireModifiers {
    pub const NONE: Self = Self(0);
    pub const SHIFT: Self = Self(1);
    pub const CONTROL: Self = Self(2);
    pub const ALT: Self = Self(4);
    pub const SUPER: Self = Self(8);
    pub const HYPER: Self = Self(16);
    pub const META: Self = Self(32);

    pub const fn from_bits_retain(bits: u8) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Converts to host modifiers while preserving bits unknown to this build.
    pub fn to_host(self) -> ratatui::crossterm::event::KeyModifiers {
        ratatui::crossterm::event::KeyModifiers::from_bits_retain(self.bits())
    }

    /// Converts host modifiers while preserving bits unknown to this build.
    pub fn from_host(modifiers: ratatui::crossterm::event::KeyModifiers) -> Self {
        Self::from_bits_retain(modifiers.bits())
    }
}

impl std::ops::BitOr for WireModifiers {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for WireModifiers {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Pane-domain input after the client has classified and consumed shell actions.
///
/// Keys are semantic rather than outer-terminal VT bytes so the target pane can
/// encode them for the child application's negotiated keyboard protocol.
/// A key carries no physical key identity: the Linux host terminal reports
/// none, so a key is identified by its code alone, and a press that committed
/// `generated_text` gets no release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientPaneInputEvent {
    Key {
        code: ClientKeyCode,
        modifiers: WireModifiers,
        kind: ClientKeyKind,
        repeat_count: u16,
        shifted_codepoint: Option<char>,
        generated_text: Option<String>,
    },
    TextCommit(String),
    Mouse {
        kind: ClientMouseKind,
        position: ClientMousePosition,
        geometry: Option<ClientMouseGeometry>,
        modifiers: WireModifiers,
        lines: u16,
    },
    Paste(String),
}

/// Messages sent from the client to the server over the client protocol socket.
/// Not `Eq`: an endpoint command can carry a split ratio.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Graceful disconnect request.
    Detach,

    /// Resize the pane viewport of a client-owned shell.
    ClientShellResize { geometry: super::TerminalGeometry },

    /// Deliver client-classified semantic input directly to a stable pane target.
    ClientShellPaneInput {
        pane_id: PublicPaneId,
        #[serde(
            serialize_with = "codec::serialize_bounded_vec::<MAX_INPUT_EVENT_BATCH, _, _>",
            deserialize_with = "codec::deserialize_bounded_vec::<MAX_INPUT_EVENT_BATCH, _, _>"
        )]
        events: Vec<ClientPaneInputEvent>,
    },

    /// Invoke one endpoint operation through this client shell's selected
    /// connection. The server answers with one `ClientShellEndpointResponse`
    /// naming the same boot and request id.
    ClientShellEndpointRequest {
        boot_id: BootId,
        request_id: RequestId,
        command: crate::command::EndpointCommand,
    },

    /// Publish one host terminal color or appearance update observed by a client-owned shell.
    ClientShellHostTheme { update: ClientHostThemeUpdate },

    /// Publish whether the outer terminal containing a client shell has focus.
    ClientShellFocus { focused: bool },

    /// Open a client-owned shell connection.
    EndpointHello(super::endpoint::EndpointClientHello),
    /// Replay this viewed connection's current mouse capture, keyboard mode and title.
    ReplayHostEffects,
    /// Check that a connected endpoint is responsive.
    HealthPing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHostColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientHostDefaultColorKind {
    Foreground,
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientHostAppearance {
    Dark,
    Light,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientHostThemeUpdate {
    DefaultColor {
        kind: ClientHostDefaultColorKind,
        color: ClientHostColor,
    },
    PaletteColors(
        #[serde(
            serialize_with = "codec::serialize_bounded_vec::<PALETTE_COLOR_COUNT, _, _>",
            deserialize_with = "codec::deserialize_bounded_vec::<PALETTE_COLOR_COUNT, _, _>"
        )]
        Vec<(u8, ClientHostColor)>,
    ),
    Appearance(ClientHostAppearance),
}

#[cfg(test)]
mod host_mapping_tests {
    use super::*;
    use ratatui::crossterm::event::{
        KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    };

    #[test]
    fn every_wire_key_code_round_trips_through_the_host_model() {
        let codes = [
            ClientKeyCode::Backspace,
            ClientKeyCode::Enter,
            ClientKeyCode::Left,
            ClientKeyCode::Right,
            ClientKeyCode::Up,
            ClientKeyCode::Down,
            ClientKeyCode::Home,
            ClientKeyCode::End,
            ClientKeyCode::PageUp,
            ClientKeyCode::PageDown,
            ClientKeyCode::Tab,
            ClientKeyCode::BackTab,
            ClientKeyCode::Delete,
            ClientKeyCode::Insert,
            ClientKeyCode::Esc,
            ClientKeyCode::Char('x'),
            ClientKeyCode::F(12),
            ClientKeyCode::Null,
        ];
        for code in codes {
            assert_eq!(ClientKeyCode::from_host(code.to_host()), Some(code.clone()));
        }
        assert_eq!(ClientKeyCode::from_host(KeyCode::CapsLock), None);
    }

    #[test]
    fn key_kinds_mouse_kinds_and_buttons_round_trip_through_the_host_model() {
        for kind in [
            KeyEventKind::Press,
            KeyEventKind::Repeat,
            KeyEventKind::Release,
        ] {
            assert_eq!(ClientKeyKind::from_host(kind).to_host(), kind);
        }
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            assert_eq!(ClientMouseButton::from_host(button).to_host(), button);
            for kind in [
                MouseEventKind::Down(button),
                MouseEventKind::Up(button),
                MouseEventKind::Drag(button),
            ] {
                assert_eq!(ClientMouseKind::from_host(kind).to_host(), kind);
            }
        }
        for kind in [
            MouseEventKind::Moved,
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
        ] {
            assert_eq!(ClientMouseKind::from_host(kind).to_host(), kind);
        }
    }

    /// Named wire bits match crossterm's, and bits this build does not name
    /// survive both directions.
    #[test]
    fn modifiers_keep_named_and_unknown_bits_in_both_directions() {
        for (wire, host) in [
            (WireModifiers::SHIFT, KeyModifiers::SHIFT),
            (WireModifiers::CONTROL, KeyModifiers::CONTROL),
            (WireModifiers::ALT, KeyModifiers::ALT),
            (WireModifiers::SUPER, KeyModifiers::SUPER),
            (WireModifiers::HYPER, KeyModifiers::HYPER),
            (WireModifiers::META, KeyModifiers::META),
        ] {
            assert_eq!(wire.to_host(), host);
            assert_eq!(WireModifiers::from_host(host), wire);
        }
        let unknown = WireModifiers::from_bits_retain(0b1100_0001);
        assert_eq!(WireModifiers::from_host(unknown.to_host()), unknown);
    }

    #[test]
    fn text_bytes_charges_repeated_generated_text_and_pastes() {
        let key = ClientPaneInputEvent::Key {
            code: ClientKeyCode::Char('a'),
            modifiers: WireModifiers::NONE,
            kind: ClientKeyKind::Press,
            repeat_count: 3,
            shifted_codepoint: None,
            generated_text: Some("ab".into()),
        };
        assert_eq!(key.text_bytes(), 6);
        assert_eq!(ClientPaneInputEvent::Paste("hello".into()).text_bytes(), 5);
    }
}
