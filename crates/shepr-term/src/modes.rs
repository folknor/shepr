//! Terminal modes and the keyboard protocol state a child negotiates.

use crate::seq;

/// A DEC private mode supported by the terminal adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// The discriminant indexes the corresponding entry in the emulator's mode table.
#[repr(usize)]
pub enum DecMode {
    ApplicationCursorKeys,
    ColumnMode,
    Origin,
    LineWrap,
    X10Mouse,
    CursorBlink,
    ShowCursor,
    MousePressRelease,
    MouseButtonMotion,
    MouseAnyMotion,
    FocusEvents,
    MouseUtf8,
    MouseSgr,
    MouseAlternateScroll,
    MouseSgrPixels,
    UrgencyHints,
    AlternateScreen,
    BracketedPaste,
    SynchronizedOutput,
    ColorSchemeReport,
    InBandResize,
}

impl DecMode {
    /// The DEC private mode number used in escape sequences.
    pub const fn number(self) -> u16 {
        match self {
            Self::ApplicationCursorKeys => 1,
            Self::ColumnMode => 3,
            Self::Origin => 6,
            Self::LineWrap => 7,
            Self::X10Mouse => 9,
            Self::CursorBlink => 12,
            Self::ShowCursor => 25,
            Self::MousePressRelease => 1000,
            Self::MouseButtonMotion => 1002,
            Self::MouseAnyMotion => 1003,
            Self::FocusEvents => 1004,
            Self::MouseUtf8 => 1005,
            Self::MouseSgr => 1006,
            Self::MouseAlternateScroll => 1007,
            Self::MouseSgrPixels => 1016,
            Self::UrgencyHints => 1042,
            Self::AlternateScreen => 1049,
            Self::BracketedPaste => 2004,
            Self::SynchronizedOutput => 2026,
            Self::ColorSchemeReport => 2031,
            Self::InBandResize => 2048,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusEvent {
    Gained,
    Lost,
}

pub fn encode_focus(event: FocusEvent) -> &'static [u8] {
    match event {
        FocusEvent::Gained => seq::FOCUS_GAINED,
        FocusEvent::Lost => seq::FOCUS_LOST,
    }
}

/// Kitty keyboard mode flags reported by the terminal core and shared with
/// the wire and host-terminal adapters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct KittyKeyboardFlags(u16);

impl KittyKeyboardFlags {
    pub const NONE: Self = Self(0);
    pub const DISAMBIGUATE: Self = Self(1);
    pub const REPORT_EVENT_TYPES: Self = Self(2);
    pub const REPORT_ALTERNATE_KEYS: Self = Self(4);
    pub const REPORT_ALL_KEYS: Self = Self(8);
    pub const REPORT_ASSOCIATED_TEXT: Self = Self(16);

    pub const fn from_bits_retain(bits: u16) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u16 {
        self.0
    }

    pub const fn contains(self, flags: Self) -> bool {
        self.0 & flags.0 == flags.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn insert(&mut self, flags: Self) {
        self.0 |= flags.0;
    }
}

impl std::ops::BitOr for KittyKeyboardFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for KittyKeyboardFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// The three xterm modifyOtherKeys levels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ModifyOtherKeysLevel {
    #[default]
    Off,
    ExceptWellDefined,
    All,
}

impl ModifyOtherKeysLevel {
    pub const fn set_sequence(self) -> &'static [u8] {
        match self {
            Self::Off => b"\x1b[>4;0m",
            Self::ExceptWellDefined => b"\x1b[>4;1m",
            Self::All => b"\x1b[>4;2m",
        }
    }
}
