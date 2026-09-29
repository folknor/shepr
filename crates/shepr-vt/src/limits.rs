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
// limits-exempt: decimal syntax for the u16 terminal parameter has a fixed width.
pub(crate) const MAX_U16_DECIMAL_DIGITS: usize = 5;

/// Fixed framing bytes reserved when sizing an XTGETTCAP reply, including its
/// prefix, value separator, and terminator.
// limits-exempt: XTGETTCAP's response framing is part of the terminal protocol format.
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

/// Maximum active keyboard-mode stack depth accepted by the adapter. The
/// pinned alacritty parser has a fixed cap with a broken overflow branch;
/// matching that depth lets shepr reject the next push before it reaches that
/// branch.
pub(crate) const KEYBOARD_MODE_STACK_MAX_DEPTH: usize = 4096;

/// Maximum width in terminal cells for one Unicode codepoint. Terminal cells
/// use the maximum width defined by Unicode display width.
// limits-exempt: Unicode display width is fixed by the text-width model.
pub(crate) const MAX_UNICODE_CODEPOINT_WIDTH: u8 = 2;

/// Red coefficient in the integer RGB luminance approximation used to infer
/// whether a color is light. The standard weighted sum gives green the greatest
/// contribution.
// limits-exempt: fixed coefficient in the standard integer RGB luminance approximation.
pub(crate) const LUMINANCE_RED_WEIGHT: u32 = 299;

/// Green coefficient in the integer RGB luminance approximation; see
/// `LUMINANCE_RED_WEIGHT` for the weighted-sum scale and rationale.
// limits-exempt: fixed coefficient in the standard integer RGB luminance approximation.
pub(crate) const LUMINANCE_GREEN_WEIGHT: u32 = 587;

/// Blue coefficient in the integer RGB luminance approximation; see
/// `LUMINANCE_RED_WEIGHT` for the weighted-sum scale and rationale.
// limits-exempt: fixed coefficient in the standard integer RGB luminance approximation.
pub(crate) const LUMINANCE_BLUE_WEIGHT: u32 = 114;

/// Weighted luminance threshold for classifying an RGB color as light. The
/// threshold is near the midpoint of the weighted 8-bit RGB range.
pub(crate) const LIGHT_LUMINANCE_THRESHOLD: u32 = 128_000;
