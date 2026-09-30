//! The DEC private modes the terminal core knows, in one table: the typed
//! mode, its name, and how its live state is read.
//!
//! Modes alacritty implements are read from `TermMode` (or the cursor style
//! for 12). Modes the parser passes through as unknown (9, 1016, 2031, 2048)
//! are stored in the adapter's [`ExtraModes`]; the handler owns their side
//! effects. 2026 is parser state and reads through the synchronized-output
//! deadline.
//!
//! A number missing from the table is unsupported for both query and write.
//! That includes 47 and 1047: vte only implements the 1049 screen swap, so
//! the other alternate-screen spellings neither switch screens nor report the
//! 1049 state. 3 (DECCOLM) performs alacritty's page reset but reports
//! unsupported because it stores no mode state.
//!
//! Number lookups scan the short static slice without allocation or locking.

use super::ExtraModes;
use alacritty_terminal::term::TermMode;

/// A DEC private mode supported by the terminal adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// The discriminant indexes the corresponding entry in `MODES`.
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
#[expect(
    variant_size_differences,
    reason = "a Copy mode-table entry of eight bytes; the terminal mode flags are the largest"
)]
pub(super) enum Getter {
    Term(TermMode),
    CursorBlink,
    Extra(ExtraMode),
    SynchronizedOutput,
    /// Parsed but always reported as unsupported.
    Unsupported,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ModeSpec {
    pub(super) mode: DecMode,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "documents the table; read by tests")
    )]
    pub(super) name: &'static str,
    pub(super) get: Getter,
    pub(super) extra: Option<ExtraMode>,
}

const fn term_mode(mode: DecMode, name: &'static str, term_mode: TermMode) -> ModeSpec {
    ModeSpec {
        mode,
        name,
        get: Getter::Term(term_mode),
        extra: None,
    }
}

const fn extra(mode: DecMode, name: &'static str, extra: ExtraMode) -> ModeSpec {
    ModeSpec {
        mode,
        name,
        get: Getter::Extra(extra),
        extra: Some(extra),
    }
}

pub(super) const MODES: &[ModeSpec] = &[
    term_mode(
        DecMode::ApplicationCursorKeys,
        "application cursor keys",
        TermMode::APP_CURSOR,
    ),
    ModeSpec {
        mode: DecMode::ColumnMode,
        name: "column mode",
        get: Getter::Unsupported,
        extra: None,
    },
    term_mode(DecMode::Origin, "origin", TermMode::ORIGIN),
    term_mode(DecMode::LineWrap, "line wrap", TermMode::LINE_WRAP),
    extra(DecMode::X10Mouse, "x10 mouse", ExtraMode::X10Mouse),
    ModeSpec {
        mode: DecMode::CursorBlink,
        name: "cursor blink",
        get: Getter::CursorBlink,
        extra: None,
    },
    term_mode(DecMode::ShowCursor, "show cursor", TermMode::SHOW_CURSOR),
    term_mode(
        DecMode::MousePressRelease,
        "mouse clicks",
        TermMode::MOUSE_REPORT_CLICK,
    ),
    term_mode(
        DecMode::MouseButtonMotion,
        "mouse drag",
        TermMode::MOUSE_DRAG,
    ),
    term_mode(
        DecMode::MouseAnyMotion,
        "mouse motion",
        TermMode::MOUSE_MOTION,
    ),
    term_mode(DecMode::FocusEvents, "focus events", TermMode::FOCUS_IN_OUT),
    term_mode(DecMode::MouseUtf8, "utf-8 mouse", TermMode::UTF8_MOUSE),
    term_mode(DecMode::MouseSgr, "sgr mouse", TermMode::SGR_MOUSE),
    term_mode(
        DecMode::MouseAlternateScroll,
        "alternate scroll",
        TermMode::ALTERNATE_SCROLL,
    ),
    extra(
        DecMode::MouseSgrPixels,
        "sgr pixel mouse",
        ExtraMode::SgrPixelsMouse,
    ),
    term_mode(
        DecMode::UrgencyHints,
        "urgency hints",
        TermMode::URGENCY_HINTS,
    ),
    term_mode(
        DecMode::AlternateScreen,
        "alternate screen",
        TermMode::ALT_SCREEN,
    ),
    term_mode(
        DecMode::BracketedPaste,
        "bracketed paste",
        TermMode::BRACKETED_PASTE,
    ),
    ModeSpec {
        mode: DecMode::SynchronizedOutput,
        name: "synchronized output",
        get: Getter::SynchronizedOutput,
        extra: None,
    },
    extra(
        DecMode::ColorSchemeReport,
        "color scheme report",
        ExtraMode::ColorSchemeReport,
    ),
    extra(
        DecMode::InBandResize,
        "in-band resize",
        ExtraMode::InBandResize,
    ),
];

// `lookup` indexes the table by discriminant: every entry must sit at its own
// variant's index, and the table must end at the last variant, so the table
// order matches the enum order and every variant has an entry.
const _: () = {
    assert!(MODES.len() == DecMode::InBandResize as usize + 1);
    let mut index = 0;
    while index < MODES.len() {
        assert!(MODES[index].mode as usize == index);
        index += 1;
    }
};

pub(super) fn lookup(mode: DecMode) -> &'static ModeSpec {
    &MODES[mode as usize]
}

pub(super) fn lookup_number(number: u16) -> Option<&'static ModeSpec> {
    MODES.iter().find(|spec| spec.mode.number() == number)
}

/// The adapter-stored mode for a number vte passed through as unknown.
pub(super) fn extra_mode(number: u16) -> Option<ExtraMode> {
    lookup_number(number)?.extra
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_table_numbers_are_unique_and_named() {
        for (index, spec) in MODES.iter().enumerate() {
            assert!(!spec.name.is_empty());
            assert!(
                MODES[index + 1..].iter().all(|other| {
                    other.mode != spec.mode && other.mode.number() != spec.mode.number()
                }),
                "duplicate mode {}",
                spec.mode.number()
            );
        }
    }

    #[test]
    fn alternate_screen_aliases_are_unsupported() {
        assert!(lookup_number(47).is_none());
        assert!(lookup_number(1047).is_none());
        assert!(lookup_number(1049).is_some());
    }

    #[test]
    fn each_dec_mode_indexes_its_table_entry() {
        for spec in MODES {
            assert_eq!(lookup(spec.mode).mode, spec.mode);
        }
    }
}
