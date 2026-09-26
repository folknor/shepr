//! The DEC private modes the terminal core knows, in one table: the number,
//! a name, how the live state is read, and how a write is routed.
//!
//! Modes alacritty implements are written through vte's `NamedPrivateMode`
//! and read from `TermMode` (or the cursor style for 12). Modes alacritty
//! does not know (9, 1016, 2031, 2048) arrive from vte as
//! `PrivateMode::Unknown` and are stored in the adapter's [`ExtraModes`]; the
//! handler owns their side effects. 2026 is parser state and reads through
//! the synchronized-output deadline.
//!
//! A number missing from the table is unsupported for both query and write.
//! That includes 47 and 1047: vte only implements the 1049 screen swap, so
//! the other alternate-screen spellings neither switch screens nor report the
//! 1049 state. 3 (DECCOLM) is written through vte but reports unsupported,
//! because alacritty only performs its side effects and keeps no state.
//!
//! Lookups are a linear scan over a short static slice: no allocation and no
//! locking on the parsing path.

use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::NamedPrivateMode;

use super::ExtraModes;

/// A mode alacritty does not model, stored in [`ExtraModes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExtraMode {
    X10Mouse,
    SgrPixelsMouse,
    ColorSchemeReport,
    InBandResize,
}

impl ExtraMode {
    pub(super) fn get(self, modes: &ExtraModes) -> bool {
        match self {
            Self::X10Mouse => modes.x10_mouse,
            Self::SgrPixelsMouse => modes.sgr_pixels_mouse,
            Self::ColorSchemeReport => modes.color_scheme_report,
            Self::InBandResize => modes.in_band_resize,
        }
    }

    pub(super) fn set(self, modes: &mut ExtraModes, value: bool) {
        let field = match self {
            Self::X10Mouse => &mut modes.x10_mouse,
            Self::SgrPixelsMouse => &mut modes.sgr_pixels_mouse,
            Self::ColorSchemeReport => &mut modes.color_scheme_report,
            Self::InBandResize => &mut modes.in_band_resize,
        };
        *field = value;
    }
}

/// How a mode's current value is read.
#[derive(Debug, Clone, Copy)]
pub(super) enum Getter {
    Term(TermMode),
    CursorBlink,
    Extra(ExtraMode),
    SynchronizedOutput,
    /// Writable but always reported as unsupported.
    Unsupported,
}

/// How a write reaches the terminal.
#[derive(Debug, Clone, Copy)]
pub(super) enum Setter {
    Vte(NamedPrivateMode),
    Extra(ExtraMode),
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ModeSpec {
    pub(super) number: u16,
    #[allow(dead_code)] // Documents the table; read by tests.
    pub(super) name: &'static str,
    pub(super) get: Getter,
    pub(super) set: Setter,
}

const fn vte(number: u16, name: &'static str, mode: TermMode, named: NamedPrivateMode) -> ModeSpec {
    ModeSpec {
        number,
        name,
        get: Getter::Term(mode),
        set: Setter::Vte(named),
    }
}

const fn extra(number: u16, name: &'static str, mode: ExtraMode) -> ModeSpec {
    ModeSpec {
        number,
        name,
        get: Getter::Extra(mode),
        set: Setter::Extra(mode),
    }
}

pub(super) const MODES: &[ModeSpec] = &[
    vte(
        1,
        "application cursor keys",
        TermMode::APP_CURSOR,
        NamedPrivateMode::CursorKeys,
    ),
    ModeSpec {
        number: 3,
        name: "column mode",
        get: Getter::Unsupported,
        set: Setter::Vte(NamedPrivateMode::ColumnMode),
    },
    vte(6, "origin", TermMode::ORIGIN, NamedPrivateMode::Origin),
    vte(
        7,
        "line wrap",
        TermMode::LINE_WRAP,
        NamedPrivateMode::LineWrap,
    ),
    extra(9, "x10 mouse", ExtraMode::X10Mouse),
    ModeSpec {
        number: super::MODE_CURSOR_BLINK,
        name: "cursor blink",
        get: Getter::CursorBlink,
        set: Setter::Vte(NamedPrivateMode::BlinkingCursor),
    },
    vte(
        25,
        "show cursor",
        TermMode::SHOW_CURSOR,
        NamedPrivateMode::ShowCursor,
    ),
    vte(
        1000,
        "mouse clicks",
        TermMode::MOUSE_REPORT_CLICK,
        NamedPrivateMode::ReportMouseClicks,
    ),
    vte(
        1002,
        "mouse drag",
        TermMode::MOUSE_DRAG,
        NamedPrivateMode::ReportCellMouseMotion,
    ),
    vte(
        1003,
        "mouse motion",
        TermMode::MOUSE_MOTION,
        NamedPrivateMode::ReportAllMouseMotion,
    ),
    vte(
        1004,
        "focus events",
        TermMode::FOCUS_IN_OUT,
        NamedPrivateMode::ReportFocusInOut,
    ),
    vte(
        1005,
        "utf-8 mouse",
        TermMode::UTF8_MOUSE,
        NamedPrivateMode::Utf8Mouse,
    ),
    vte(
        1006,
        "sgr mouse",
        TermMode::SGR_MOUSE,
        NamedPrivateMode::SgrMouse,
    ),
    vte(
        1007,
        "alternate scroll",
        TermMode::ALTERNATE_SCROLL,
        NamedPrivateMode::AlternateScroll,
    ),
    extra(1016, "sgr pixel mouse", ExtraMode::SgrPixelsMouse),
    vte(
        super::MODE_URGENCY_HINTS,
        "urgency hints",
        TermMode::URGENCY_HINTS,
        NamedPrivateMode::UrgencyHints,
    ),
    vte(
        1049,
        "alternate screen",
        TermMode::ALT_SCREEN,
        NamedPrivateMode::SwapScreenAndSetRestoreCursor,
    ),
    vte(
        2004,
        "bracketed paste",
        TermMode::BRACKETED_PASTE,
        NamedPrivateMode::BracketedPaste,
    ),
    ModeSpec {
        number: super::MODE_SYNCHRONIZED_OUTPUT,
        name: "synchronized output",
        get: Getter::SynchronizedOutput,
        set: Setter::Vte(NamedPrivateMode::SyncUpdate),
    },
    extra(2031, "color scheme report", ExtraMode::ColorSchemeReport),
    extra(2048, "in-band resize", ExtraMode::InBandResize),
];

pub(super) fn lookup(number: u16) -> Option<&'static ModeSpec> {
    MODES.iter().find(|spec| spec.number == number)
}

/// The adapter-stored mode for a number vte passed through as unknown.
pub(super) fn extra_mode(number: u16) -> Option<ExtraMode> {
    match lookup(number)?.set {
        Setter::Extra(mode) => Some(mode),
        Setter::Vte(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_table_numbers_are_unique_and_named() {
        for (index, spec) in MODES.iter().enumerate() {
            assert!(!spec.name.is_empty());
            assert!(
                MODES[index + 1..]
                    .iter()
                    .all(|other| other.number != spec.number),
                "duplicate mode {}",
                spec.number
            );
        }
    }

    #[test]
    fn alternate_screen_aliases_are_unsupported() {
        assert!(lookup(47).is_none());
        assert!(lookup(1047).is_none());
        assert!(lookup(1049).is_some());
    }
}
