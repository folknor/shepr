use shepr_core::limits::KIBIBYTE_BYTES;

/// Maximum CSI bytes retained by the scanner while framing one sequence. The
/// fixed bound keeps malformed or unusually long controls from growing a
/// buffer for every pane while leaving room for the CSI forms shepr handles.
pub(crate) const MAX_CSI_BYTES: usize = 64;

/// Maximum OSC bytes retained by the scanner. A PATH_MAX path expands when
/// percent-encoded as a file URI, before the authority and OSC prefix, so this
/// leaves room for a working-directory report while bounding per-pane buffering.
pub(crate) const MAX_OSC_BYTES: usize = 16 * KIBIBYTE_BYTES;

/// Maximum bytes retained for a DCS introducer before the scanner drops it.
/// Inspected introductions are short; the bound limits ignored input without
/// restricting the XTGETTCAP payload, which has its own limit below.
pub(crate) const MAX_DCS_INTRO_BYTES: usize = 16;

/// Maximum XTGETTCAP payload bytes retained by the scanner for one DCS. The
/// ceiling allows long capability names while keeping attacker supplied
/// terminal input bounded per pane.
pub(crate) const MAX_XTGETTCAP_BYTES: usize = KIBIBYTE_BYTES;

/// Maximum decimal digits accepted before parsing a terminal parameter as a
/// `u16`; the limit covers its full range while rejecting longer spellings,
/// including unnecessarily padded values.
pub(crate) const MAX_U16_DECIMAL_DIGITS: usize = 5;

/// Fixed framing bytes reserved when sizing an XTGETTCAP reply. Keeping the
/// prefix, value separator, and terminator outside the payload budget accounts
/// for the bytes the wire format adds around capability data.
pub(crate) const XTGETTCAP_REPLY_OVERHEAD_BYTES: usize = 8;

/// Minimum columns used when converting a byte scrollback budget to lines, so
/// a zero-sized caller input cannot make the estimated line size zero.
pub(crate) const MIN_SCROLLBACK_COLUMNS: usize = 1;

/// Minimum cell bytes used in the scrollback estimate to keep division safe
/// even if the cell representation ever becomes zero-sized.
pub(crate) const MIN_SCROLLBACK_CELL_BYTES: usize = 1;

/// Minimum line count retained for any non-zero scrollback byte budget. This
/// keeps tiny byte budgets useful for scrolling, even on wide panes.
pub(crate) const MIN_SCROLLBACK_LINES: usize = 1_000;

/// Maximum line count produced from a byte scrollback budget. The byte budget
/// alone does not bound heap-held cell extras or history retained across a
/// widening resize, so this caps the core's retained row count.
pub(crate) const MAX_SCROLLBACK_LINES: usize = 1_000_000;

/// Maximum OSC 52 clipboard payload accepted from a child terminal. The
/// ceiling permits large text selections while bounding the payload that
/// the parser hands to its caller.
pub(crate) const MAX_CLIPBOARD_BYTES: usize = 192 * KIBIBYTE_BYTES;

/// Maximum bytes vte may retain in an OSC raw payload before the adapter ends
/// the sequence early. Parameter separators are excluded because vte stores
/// them as boundaries, not payload bytes. Twice the base64 length of the
/// largest accepted clipboard store: every store shepr accepts reaches the
/// parser whole, and an OSC 52 cut at this bound still decodes to more than
/// `MAX_CLIPBOARD_BYTES`, so it is dropped by size instead of stored truncated.
pub(crate) const MAX_OSC_RAW_BYTES: usize = 2 * 4 * MAX_CLIPBOARD_BYTES.div_ceil(3);

/// Maximum bytes of a window title handed to alacritty. Its `Term` keeps the
/// title uncapped and `CSI 22 t` clones it onto a title stack up to 4096 deep,
/// so a title of up to `MAX_OSC_RAW_BYTES` pushed repeatedly (5 bytes per push)
/// would hold gigabytes per pane. The cap is far above what any title display
/// or detection needs; a longer title is cut on a character boundary.
pub(crate) const MAX_TITLE_BYTES: usize = 4 * KIBIBYTE_BYTES;

/// Maximum active keyboard-mode stack depth accepted by the adapter. The
/// pinned alacritty parser has a fixed cap with a broken overflow branch;
/// matching that depth lets shepr reject the next push before it reaches that
/// branch.
pub(crate) const KEYBOARD_MODE_STACK_MAX_DEPTH: usize = 4096;

/// Maximum width in terminal cells for one Unicode codepoint. The cap matches
/// the widest category in Unicode display width, keeping cell accounting within
/// that model.
pub(crate) const MAX_UNICODE_CODEPOINT_WIDTH: u8 = 2;
