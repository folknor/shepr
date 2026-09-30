use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Server → Client messages
// ---------------------------------------------------------------------------

/// A terminal color represented without packing a tag into a scalar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireColor {
    Reset,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    White,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Cell style flags sent with a rendered frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WireStyleFlags(u8);

impl WireStyleFlags {
    pub const BOLD: Self = Self(1 << 0);
    pub const DIM: Self = Self(1 << 1);
    pub const ITALIC: Self = Self(1 << 2);
    pub const SLOW_BLINK: Self = Self(1 << 3);
    pub const RAPID_BLINK: Self = Self(1 << 4);
    pub const REVERSED: Self = Self(1 << 5);
    pub const HIDDEN: Self = Self(1 << 6);
    pub const CROSSED_OUT: Self = Self(1 << 7);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn toggle(&mut self, flag: Self) {
        self.0 ^= flag.0;
    }
}

/// Semantic cell style, with underline shape independent from the flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WireStyle {
    pub flags: WireStyleFlags,
    pub underline: shepr_vt::UnderlineStyle,
}
