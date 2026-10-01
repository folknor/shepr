//! Input framing limits and terminal-facing buffer sizes.

use std::time::Duration;

/// Idle time before an incomplete raw terminal key sequence is flushed.
///
/// A short delay keeps lone Escape responsive while allowing bytes from one
/// terminal write to arrive together.
pub const RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS: i32 = 10;

/// Wait this long before flushing a possible mouse sequence when host mouse
/// reporting is active; the interval accommodates fragmented reports.
pub const MOUSE_ACTIVE_ESCAPE_SEQUENCE_FLUSH_TIMEOUT_MS: i32 = 150;

/// Wait this long before flushing a lone `ESC [` when host mouse reporting is
/// active: long enough for a mouse report split right after its introducer
/// (seen 33 ms apart), short enough that a legacy Alt+[ is not glued to the
/// next key.
pub const MOUSE_ACTIVE_CSI_INTRODUCER_FLUSH_TIMEOUT_MS: i32 = 50;

/// How long a mouse report prefix is kept after it outlived keyboard timing
/// while the host sends Escape disambiguated (`CSI 27u`), so its tail can still
/// arrive (seen 350 ms late). Any other input ends the wait early.
pub const DISAMBIGUATED_MOUSE_TAIL_FLUSH_TIMEOUT_MS: i32 = 500;

/// Largest bracketed paste body the framer holds while waiting for its
/// terminator. Past this the held part is closed and delivered as one paste and
/// the rest of it is dropped up to the terminator, so a paste that never ends
/// cannot grow the buffer without bound. It is far above the server's
/// per-message input limit, which rejects such a paste in the client shell
/// anyway (with a visible notice), so the cut only bounds the buffer.
pub(crate) const MAX_PENDING_PASTE_BYTES: usize = 16 * 1024 * 1024;

/// How long a held, unterminated bracketed paste may go without receiving a
/// byte before the framer stops waiting for its terminator. Terminals write a
/// paste in one go, so a stall this long means the terminator is not coming;
/// without the limit every later keystroke would queue behind the paste and
/// the client would look hung. The check runs when input next arrives: the
/// held part is delivered as a complete paste and the new input is framed
/// normally.
pub(crate) const PASTE_STALL_TIMEOUT: Duration = Duration::from_secs(3);

/// Number of color-query replies expected from the full host theme query.
///
/// The count includes every indexed palette entry plus foreground and
/// background replies.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the palette size plus two replies fits in u16"
)]
pub(crate) const MAX_HOST_COLOR_QUERY_REPLIES: u16 =
    shepr_core::limits::PALETTE_COLOR_COUNT as u16 + 2;

/// Maximum length of an orphaned SGR mouse tail accepted by the parser.
///
/// The bound covers decimal coordinates in a complete supported mouse report;
/// longer tails are not retained as plausible reports.
pub(crate) const MAX_ORPHANED_SGR_MOUSE_TAIL_BYTES: usize = 32;

/// Maximum bytes retained while discarding an incomplete terminal control tail.
///
/// The ceiling allows supported host replies to finish while bounding
/// malformed or unterminated control input.
pub(crate) const MAX_DISCARDED_CONTROL_TAIL_BYTES: usize = 128;

/// Maximum bytes retained for an incomplete CSI sequence.
///
/// Complete sequences are parsed before this bound is applied. The ceiling
/// accommodates supported key encodings and mouse coordinates while bounding
/// an unterminated CSI prefix.
pub(crate) const MAX_INCOMPLETE_CSI_BYTES: usize = 128;

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
