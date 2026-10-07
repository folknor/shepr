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

/// Maximum OSC 52 clipboard payload accepted from a child terminal. The
/// ceiling permits large text selections while bounding the payload that
/// the parser hands to its caller.
pub(crate) const MAX_CLIPBOARD_BYTES: usize = 192 * KIBIBYTE_BYTES;

/// Maximum bytes vte may retain in an OSC raw payload before the adapter ends
/// the sequence early. Parameter separators are excluded because vte stores
/// them as boundaries, not payload bytes. The bound lets every accepted OSC 52
/// store reach the parser whole; when a supported OSC 52 store is cut here,
/// the scanner reports a decoded-size lower bound above `MAX_CLIPBOARD_BYTES`
/// even when the truncated base64 cannot be decoded.
pub(crate) const MAX_OSC_RAW_BYTES: usize = 2 * 4 * MAX_CLIPBOARD_BYTES.div_ceil(3);

/// Maximum bytes of a window title handed to alacritty. Its `Term` keeps the
/// title uncapped and `CSI 22 t` clones it onto a title stack up to 4096 deep,
/// so a title of up to `MAX_OSC_RAW_BYTES` pushed repeatedly (5 bytes per push)
/// would hold gigabytes per pane. The cap is far above what any title display
/// or detection needs; a longer title is cut on a character boundary. This is a
/// resource limit on parser input, not a display rule: what a displayable
/// title is, and the shorter character caps on what is retained or shown, live
/// in `shepr_term::title` and its callers.
pub(crate) const MAX_TITLE_BYTES: usize = 4 * KIBIBYTE_BYTES;

/// Maximum active keyboard-mode stack depth accepted by the adapter. The
/// pinned alacritty parser has a fixed cap with a broken overflow branch;
/// matching that depth lets shepr reject the next push before it reaches that
/// branch.
pub(crate) const KEYBOARD_MODE_STACK_MAX_DEPTH: usize = 4096;
