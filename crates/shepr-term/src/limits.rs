//! Width bounds and child-facing encoder buffer sizes.

/// Maximum width in terminal cells for one Unicode codepoint. The cap matches
/// the widest category in Unicode display width, keeping cell accounting within
/// that model.
pub(crate) const MAX_UNICODE_CODEPOINT_WIDTH: u8 = 2;
/// Initial allocation for a UTF-8 mouse report.
///
/// The initial capacity fits a complete supported report, including its escape
/// prefix and encoded coordinates, without growing the common buffer.
pub(crate) const UTF8_MOUSE_REPORT_INITIAL_CAPACITY: usize = 16;

/// Initial allocation for the common kitty key encoding before optional text.
///
/// The initial capacity avoids growth for ordinary key sequences while
/// allowing the associated-text path to expand when needed.
pub(crate) const KITTY_KEY_SEQUENCE_INITIAL_CAPACITY: usize = 32;
