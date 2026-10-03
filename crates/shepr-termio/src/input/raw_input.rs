use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use shepr_protocol::KittyKeyboardFlags;

use super::tables::{
    MOUSE_BUTTON_FIELD_MASK, MOUSE_BUTTON_RELEASE, MOUSE_DRAG_BIT,
    MOUSE_EXTENDED_BUTTON_FIELD_MASK, MOUSE_EXTENDED_BUTTON_SHIFT, mouse_button_from_code,
    mouse_modifiers_from_bits, mouse_scroll_from_code,
};
use crate::host_term::theme::{
    DefaultColorKind, HostAppearance, RgbColor, parse_default_color_response,
    parse_palette_color_response,
};
use crate::input::{TerminalKey, parse_terminal_key_sequence};
use crate::limits::{
    DISAMBIGUATED_MOUSE_TAIL_FLUSH_TIMEOUT_MS, MAX_DISCARDED_CONTROL_TAIL_BYTES,
    MAX_HOST_COLOR_QUERY_REPLIES, MAX_INCOMPLETE_CSI_BYTES, MAX_ORPHANED_SGR_MOUSE_TAIL_BYTES,
    MAX_PENDING_PASTE_BYTES, PASTE_STALL_TIMEOUT, RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS,
};

// limits-exempt: ESC is the terminal-control introducer byte used by this parser.
const ESC: u8 = 0x1b;
pub const GHOSTTY_COLOR_SCHEME_DARK_REPORT: &[u8] = b"\x1b[?997;1n";
pub const GHOSTTY_COLOR_SCHEME_LIGHT_REPORT: &[u8] = b"\x1b[?997;2n";
pub const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
pub const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

/// Length of the longest proper prefix of `needle` that `haystack` ends with,
/// so a terminator split across reads is not lost when the rest is dropped.
fn partial_suffix_len(haystack: &[u8], needle: &[u8]) -> usize {
    (1..needle.len())
        .rev()
        .find(|len| haystack.ends_with(&needle[..*len]))
        .unwrap_or(0)
}

#[derive(Debug)]
pub enum RawInputEvent {
    Key(TerminalKey),
    Paste(String),
    Mouse(MouseEvent),
    OuterFocusGained,
    OuterFocusLost,
    HostDefaultColor {
        kind: DefaultColorKind,
        color: RgbColor,
    },
    HostPaletteColors {
        colors: Vec<(u8, RgbColor)>,
    },
    HostColorSchemeChanged(HostAppearance),
    HostCellSizeReport {
        width_px: u32,
        height_px: u32,
    },
    Unsupported,
}

#[derive(Debug)]
pub struct FramedRawInputEvent {
    pub raw: Vec<u8>,
    pub event: RawInputEvent,
}

#[derive(Default)]
pub struct HostKeyboardProbeResponses {
    pub flags: Option<KittyKeyboardFlags>,
    pub primary_device_attributes: bool,
}

enum HostKeyboardProbeResponse {
    Flags(KittyKeyboardFlags),
    PrimaryDeviceAttributes,
}

/// Removes completed responses to the startup keyboard probe while preserving
/// every other host byte. Opaque strings use the same terminator scanner as the
/// normal input framer, so a response-shaped payload cannot escape a paste or
/// control string.
pub fn consume_host_keyboard_probe_responses(
    buffered_input: &mut Vec<u8>,
    responses: &mut HostKeyboardProbeResponses,
) {
    let mut offset = 0;
    while offset < buffered_input.len() {
        if buffered_input[offset..].starts_with(BRACKETED_PASTE_START) {
            let payload_start = offset + BRACKETED_PASTE_START.len();
            let Some(relative_end) = buffered_input[payload_start..]
                .windows(BRACKETED_PASTE_END.len())
                .position(|bytes| bytes == BRACKETED_PASTE_END)
            else {
                break;
            };
            offset = payload_start + relative_end + BRACKETED_PASTE_END.len();
            continue;
        }
        if let Some(control) = control_string(&buffered_input[offset..]) {
            let ControlString::Complete { len, .. } = control else {
                break;
            };
            offset += len;
            continue;
        }
        if !buffered_input[offset..].starts_with(b"\x1b[?") {
            offset += 1;
            continue;
        }

        let Some(sequence_len) = complete_escape_sequence_len(&buffered_input[offset..]) else {
            break;
        };
        let end = offset + sequence_len;
        let response = parse_host_keyboard_probe_response(&buffered_input[offset..end]);
        match response {
            Some(HostKeyboardProbeResponse::Flags(flags)) => {
                if !responses.primary_device_attributes {
                    responses.flags = Some(flags);
                }
                buffered_input.drain(offset..end);
            }
            Some(HostKeyboardProbeResponse::PrimaryDeviceAttributes) => {
                responses.primary_device_attributes = true;
                buffered_input.drain(offset..end);
            }
            _ => offset += 1,
        }
    }
}

/// A content-free name for an input event, for logging.
fn raw_input_event_kind(event: &RawInputEvent) -> &'static str {
    match event {
        RawInputEvent::Key(_) => "key",
        RawInputEvent::Paste(_) => "paste",
        RawInputEvent::Mouse(_) => "mouse",
        RawInputEvent::OuterFocusGained => "focus_gained",
        RawInputEvent::OuterFocusLost => "focus_lost",
        RawInputEvent::HostDefaultColor { .. } => "host_default_color",
        RawInputEvent::HostPaletteColors { .. } => "host_palette_colors",
        RawInputEvent::HostColorSchemeChanged(_) => "host_color_scheme",
        RawInputEvent::HostCellSizeReport { .. } => "host_cell_size",
        RawInputEvent::Unsupported => "unsupported",
    }
}

/// Client-side accounting for replies to queries sent to the outer terminal.
/// A default value is inactive until a query or tracking preference is set.
#[derive(Default)]
struct HostReplies {
    color: u16,
    cell_size: bool,
    appearance: bool,
    track_color_scheme: bool,
    query_appearance_on_focus: bool,
}

impl HostReplies {
    fn color_query_sent(&mut self) {
        self.color = MAX_HOST_COLOR_QUERY_REPLIES;
    }

    fn cell_size_query_sent(&mut self) {
        self.cell_size = true;
    }

    fn enable_color_scheme_tracking(&mut self) {
        self.track_color_scheme = true;
    }

    fn enable_appearance_query_on_focus(&mut self) {
        self.query_appearance_on_focus = true;
    }

    fn awaiting_reply(&self) -> bool {
        self.color > 0 || self.cell_size || self.appearance
    }

    fn awaiting_cell_size_or_appearance(&self) -> bool {
        self.cell_size || self.appearance
    }

    fn awaiting_cell_size(&self) -> bool {
        self.cell_size
    }

    fn awaiting_appearance(&self) -> bool {
        self.appearance
    }

    fn clear_cell_size_and_appearance(&mut self) {
        self.cell_size = false;
        self.appearance = false;
    }

    fn clear_cell_size(&mut self) {
        self.cell_size = false;
    }

    fn clear_appearance(&mut self) {
        self.appearance = false;
    }

    fn clear_all(&mut self) {
        self.color = 0;
        self.cell_size = false;
        self.appearance = false;
    }

    fn observe(&mut self, event: &RawInputEvent) {
        match event {
            RawInputEvent::HostDefaultColor { .. } | RawInputEvent::HostPaletteColors { .. } => {
                self.color = self.color.saturating_sub(1);
            }
            RawInputEvent::HostCellSizeReport { .. } => self.cell_size = false,
            RawInputEvent::OuterFocusGained if self.query_appearance_on_focus => {
                // The reader cannot observe whether the main-loop query write
                // succeeded. A lone Escape without a reply is held for one idle
                // flush, then released on the next.
                self.appearance = true;
            }
            RawInputEvent::HostColorSchemeChanged(_) => {
                self.appearance = false;
                if self.track_color_scheme {
                    self.color_query_sent();
                }
            }
            _ => {}
        }
    }
}

#[derive(Default)]
pub struct RawInputFramer {
    byte_framer: RawInputByteFramer,
}

impl RawInputFramer {
    pub fn push_framed(&mut self, data: &[u8]) -> Vec<FramedRawInputEvent> {
        Self::framed_events_from_chunks(self.byte_framer.push(data))
    }

    pub fn flush_timeout_framed(&mut self) -> Vec<FramedRawInputEvent> {
        Self::framed_events_from_chunks(self.byte_framer.flush_timeout())
    }

    pub fn for_host_input() -> Self {
        Self {
            byte_framer: RawInputByteFramer::for_host_input(),
        }
    }

    pub fn set_host_escape_disambiguation_active(&mut self, active: bool) {
        self.byte_framer
            .set_host_escape_disambiguation_active(active);
    }

    pub fn host_color_query_sent(&mut self) {
        self.byte_framer.host_color_query_sent();
    }

    pub fn host_cell_size_query_sent(&mut self) {
        self.byte_framer.host_cell_size_query_sent();
    }

    pub fn enable_host_color_scheme_change_tracking(&mut self) {
        self.byte_framer.enable_host_color_scheme_change_tracking();
    }

    pub fn enable_host_appearance_query_on_focus(&mut self) {
        self.byte_framer.enable_host_appearance_query_on_focus();
    }

    pub fn has_pending_input(&self) -> bool {
        self.byte_framer.has_pending_input()
    }

    pub fn has_pending_lone_escape(&self) -> bool {
        self.byte_framer.has_pending_lone_escape()
    }

    pub fn has_pending_incomplete_mouse_sequence(&self) -> bool {
        self.byte_framer.has_pending_incomplete_mouse_sequence()
    }

    /// Whether the held input is exactly `ESC [`.
    pub fn has_pending_csi_introducer(&self) -> bool {
        self.byte_framer.has_pending_csi_introducer()
    }

    /// How long to keep waiting after an idle flush held input back.
    pub fn held_input_flush_timeout_ms(&self) -> i32 {
        self.byte_framer.held_input_flush_timeout_ms()
    }

    fn framed_events_from_chunks(chunks: Vec<Vec<u8>>) -> Vec<FramedRawInputEvent> {
        chunks
            .into_iter()
            .filter_map(|chunk| {
                if chunk.as_slice() == [ESC] {
                    return Some(FramedRawInputEvent {
                        raw: chunk,
                        event: RawInputEvent::Key(TerminalKey::new(
                            crossterm::event::KeyCode::Esc,
                            KeyModifiers::empty(),
                        )),
                    });
                }
                // A chunk the idle flush released as a key (a lone `ESC [` or
                // `ESC O`, a held prefix whose tail never came) is not a
                // complete escape sequence, so the escape framing cannot read
                // it; the key parser can (Alt+[, Alt+O).
                let event = extract_one_event(&chunk)
                    .map(|(event, _consumed)| event)
                    .or_else(|| {
                        std::str::from_utf8(&chunk)
                            .ok()
                            .and_then(parse_terminal_key_sequence)
                            .map(RawInputEvent::Key)
                    })?;
                // Length and kind only: the bytes and the parsed key are
                // what the user typed, passwords included, and the log file
                // outlives the session.
                tracing::debug!(
                    len = chunk.len(),
                    kind = raw_input_event_kind(&event),
                    "raw input event parsed"
                );
                Some(FramedRawInputEvent { raw: chunk, event })
            })
            .collect()
    }
}

/// The sole owner of framing holds. Reply preferences are independent of input.
#[derive(Default)]
enum Held {
    #[default]
    None,
    /// An ordinary incomplete escape/key prefix ends at the next idle flush,
    /// which releases a parseable key or drops the prefix. An incomplete CSI
    /// also ends when it reaches MAX_INCOMPLETE_CSI_BYTES and moves to the
    /// bounded control-tail discard. Host queries or Escape disambiguation may
    /// transition it to HostReplyPrefix or MouseWait first.
    Sequence,
    /// An incomplete mouse report ends at the first idle flush: SGR becomes
    /// MouseTail, while other prefixes are released as keys or dropped. The
    /// CSI byte bound can move an overlong prefix to control-tail discard.
    /// Host Escape disambiguation may first transition it to MouseWait.
    MousePrefix,
    /// Legacy rxvt Alt+arrow may start with two Escapes. The first Escape is
    /// released at the first idle flush, or as soon as bytes rule the key out.
    /// An overlong wrapped CSI moves to the bounded control-tail discard.
    DoubledEscape,
    /// A valid incomplete UTF-8 scalar (optionally preceded by Escape) survives
    /// idle flushes by design. Continuation or invalid input ends it; storage
    /// is at most one incomplete scalar plus the optional Escape.
    Utf8,
    /// A possible host reply prefix gets one extra idle flush, then is released
    /// or becomes a bounded discarded tail. New queries do not extend it.
    HostReplyPrefix,
    /// After releasing Escape, inspect the next input for an orphaned mouse
    /// tail. A non-tail ends this marker; an incomplete tail is bounded by
    /// MAX_ORPHANED_SGR_MOUSE_TAIL_BYTES and becomes MouseTail on idle flush.
    EscapeReleased,
    /// With host Escape disambiguation, a mouse prefix gets one extra flush
    /// using DISAMBIGUATED_MOUSE_TAIL_FLUSH_TIMEOUT_MS. Non-continuation ends
    /// the wait early. Its length is just the already-buffered prefix length.
    MouseWait { prefix_len: usize },
    /// Discard only a valid continuation of this timed-out mouse prefix.
    /// Invalid input or a final byte ends it; prefix plus tail is bounded by
    /// MAX_DISCARDED_CONTROL_TAIL_BYTES. Idle alone cannot validate a report.
    MouseTail { prefix: Vec<u8> },
    /// Deliver at the terminator, cut at MAX_PENDING_PASTE_BYTES, or close
    /// after PASTE_STALL_TIMEOUT without progress when input next arrives.
    /// The byte limit is checked on each read, before holding its body again.
    /// Scanning resumes with overlap so a split terminator is not missed.
    Paste {
        scanned: usize,
        last_progress: std::time::Instant,
    },
    /// A continuing cut paste must stay discarded through its terminator,
    /// regardless of byte or flush count, so pasted text cannot become keys.
    /// Retain only a proper terminator suffix. PASTE_STALL_TIMEOUT without
    /// progress ends the discard when input next arrives.
    PasteTail { last_progress: std::time::Instant },
    /// An incomplete control string, including OSC 10/11, ends at its
    /// terminator, the first idle flush, or MAX_DISCARDED_CONTROL_TAIL_BYTES
    /// received bytes (including the introducer). Timeout or the byte bound
    /// transitions to ControlTail with a fresh tail budget, retaining a split
    /// string terminator without charging its already-received Escape twice.
    ControlString { family: ControlStringFamily },
    /// Discard through the family terminator or at most
    /// MAX_DISCARDED_CONTROL_TAIL_BYTES tail bytes, charged as they arrive.
    /// Ordinary tails also end on an implausible idle flush. Host CSI tails
    /// ignore idle by design but retain their cumulative byte bound.
    /// `charged` counts bytes still in the buffer already charged to `bytes`.
    ControlTail {
        family: ControlStringFamily,
        bytes: usize,
        charged: usize,
    },
}

#[derive(Default)]
struct RawInputByteFramer {
    buffer: Vec<u8>,
    held: Held,
    host_replies: HostReplies,
    split_coalesced_escape: bool,
    host_escape_disambiguation_active: bool,
}

impl RawInputByteFramer {
    fn for_host_input() -> Self {
        Self {
            split_coalesced_escape: true,
            ..Self::default()
        }
    }

    /// Timestamp this chunk at the input boundary; timing behavior is driven by `push_at`.
    fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.push_at(data, std::time::Instant::now())
    }

    fn push_at(&mut self, data: &[u8], now: std::time::Instant) -> Vec<Vec<u8>> {
        let mut chunks = self.give_up_stalled_paste(now);
        self.buffer.extend_from_slice(data);
        if let Held::MouseWait { prefix_len } = self.held {
            self.held = Held::Sequence;
            if !continues_escape_sequence(&self.buffer) {
                chunks.push(self.buffer.drain(..prefix_len).collect());
            }
        }
        chunks.extend(self.drain_available_chunks());
        match &mut self.held {
            Held::Paste { last_progress, .. } | Held::PasteTail { last_progress }
                if !data.is_empty() =>
            {
                *last_progress = now;
            }
            _ => {}
        }
        chunks
    }

    fn give_up_stalled_paste(&mut self, now: std::time::Instant) -> Vec<Vec<u8>> {
        let last = match self.held {
            Held::Paste { last_progress, .. } | Held::PasteTail { last_progress } => last_progress,
            _ => return Vec::new(),
        };
        if now.saturating_duration_since(last) < PASTE_STALL_TIMEOUT {
            return Vec::new();
        }
        let was_tail = matches!(self.held, Held::PasteTail { .. });
        self.held = Held::None;
        if was_tail {
            tracing::warn!("bracketed paste terminator never arrived; resuming input");
            self.buffer.clear();
            return Vec::new();
        }
        tracing::warn!(
            len = self.buffer.len(),
            "bracketed paste stalled without a terminator; delivering what arrived"
        );
        let mut paste = std::mem::take(&mut self.buffer);
        paste.extend_from_slice(BRACKETED_PASTE_END);
        vec![paste]
    }

    fn pending_paste_is_unterminated(&mut self) -> bool {
        if !self.buffer.starts_with(BRACKETED_PASTE_START) {
            return false;
        }
        let scanned = match self.held {
            Held::Paste { scanned, .. } => scanned,
            _ => 0,
        };
        let search_from = scanned
            .saturating_sub(BRACKETED_PASTE_END.len() - 1)
            .max(BRACKETED_PASTE_START.len())
            .min(self.buffer.len());
        if find_subsequence(&self.buffer[search_from..], BRACKETED_PASTE_END).is_some() {
            self.held = Held::None;
            return false;
        }
        let last_progress = match self.held {
            Held::Paste { last_progress, .. } => last_progress,
            _ => std::time::Instant::now(),
        };
        self.held = Held::Paste {
            scanned: self.buffer.len(),
            last_progress,
        };
        true
    }

    fn cut_oversized_paste(&mut self) -> Vec<u8> {
        tracing::warn!(
            len = self.buffer.len(),
            max = MAX_PENDING_PASTE_BYTES,
            "bracketed paste exceeds the held-paste limit; delivering its head and dropping the rest"
        );
        let split = self.buffer.len() - partial_suffix_len(&self.buffer, BRACKETED_PASTE_END);
        let mut paste: Vec<u8> = self.buffer.drain(..split).collect();
        paste.extend_from_slice(BRACKETED_PASTE_END);
        let last_progress = match self.held {
            Held::Paste { last_progress, .. } => last_progress,
            _ => std::time::Instant::now(),
        };
        self.held = Held::PasteTail { last_progress };
        paste
    }

    /// Hold a lone trailing ESC for one idle flush so an OSC 10/11 reply split
    /// at its ESC introducer stitches back together instead of leaking.
    fn host_color_query_sent(&mut self) {
        self.host_replies.color_query_sent();
    }

    /// Same hold window as `host_color_query_sent`, for the XTWINOPS cell size
    /// reply.
    fn host_cell_size_query_sent(&mut self) {
        self.host_replies.cell_size_query_sent();
    }

    fn enable_host_color_scheme_change_tracking(&mut self) {
        self.host_replies.enable_color_scheme_tracking();
    }

    /// Arm a possible appearance-reply window after focus gain. If no reply
    /// arrives, a lone Escape is held for one idle flush and released on the next.
    fn enable_host_appearance_query_on_focus(&mut self) {
        self.host_replies.enable_appearance_query_on_focus();
    }

    fn has_pending_input(&self) -> bool {
        !self.buffer.is_empty()
    }

    fn set_host_escape_disambiguation_active(&mut self, active: bool) {
        self.host_escape_disambiguation_active = active;
    }

    fn held_input_flush_timeout_ms(&self) -> i32 {
        if matches!(self.held, Held::MouseWait { .. }) {
            DISAMBIGUATED_MOUSE_TAIL_FLUSH_TIMEOUT_MS
        } else {
            RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS
        }
    }

    fn has_pending_csi_introducer(&self) -> bool {
        self.buffer.as_slice() == b"\x1b["
    }

    fn has_pending_lone_escape(&self) -> bool {
        self.buffer.as_slice() == [ESC]
    }

    fn has_pending_incomplete_mouse_sequence(&self) -> bool {
        starts_with_incomplete_sgr_mouse_sequence(&self.buffer)
            || starts_with_incomplete_default_mouse_sequence(&self.buffer)
    }

    fn flush_timeout(&mut self) -> Vec<Vec<u8>> {
        let mut chunks = self.drain_available_chunks();

        if matches!(self.held, Held::PasteTail { .. }) {
            return chunks;
        }

        if matches!(self.held, Held::MouseTail { .. }) {
            return chunks;
        }

        if let Held::ControlTail { family, bytes, .. } = self.held {
            if family == ControlStringFamily::HostReplyCsi {
                return chunks;
            }
            let keep_split_st = self.buffer.last() == Some(&ESC);
            let keep_discarding = plausible_control_string_tail(family, &self.buffer);
            self.buffer.clear();
            self.held = Held::None;
            if keep_discarding && bytes < MAX_DISCARDED_CONTROL_TAIL_BYTES {
                if keep_split_st {
                    self.buffer.push(ESC);
                }
                self.held = Held::ControlTail {
                    family,
                    bytes,
                    charged: self.buffer.len(),
                };
            }
            return chunks;
        }

        if self.buffer.is_empty() {
            return chunks;
        }

        // A host that disambiguates Escape sends it as `CSI 27u`, so a mouse
        // report prefix that outlives keyboard timing may still get a delayed
        // tail. Keep it for one longer wait; `push_at` releases it as a key if
        // other input follows. A finished mouse wait also counts as this
        // prefix's one-flush host reply hold, so the two holds never stack.
        let mouse_wait_served = matches!(self.held, Held::MouseWait { .. });
        if mouse_wait_served {
            self.held = Held::Sequence;
        }
        if !mouse_wait_served
            && self.host_escape_disambiguation_active
            && could_continue_as_mouse_report(&self.buffer)
        {
            tracing::trace!(
                len = self.buffer.len(),
                "holding a possible mouse report prefix for its delayed tail"
            );
            self.held = Held::MouseWait {
                prefix_len: self.buffer.len(),
            };
            return chunks;
        }

        if matches!(self.held, Held::EscapeReleased) && self.buffer.starts_with(b"[<") {
            tracing::debug!(
                len = self.buffer.len(),
                "discarding incomplete orphaned SGR mouse tail after input timeout"
            );
            let mut prefix = vec![ESC];
            prefix.append(&mut self.buffer);
            self.retain_timed_out_mouse_prefix(prefix);
            return chunks;
        }

        if starts_with_incomplete_sgr_mouse_sequence(&self.buffer) {
            tracing::debug!(
                len = self.buffer.len(),
                "discarding incomplete SGR mouse sequence after input timeout"
            );
            let prefix = std::mem::take(&mut self.buffer);
            self.retain_timed_out_mouse_prefix(prefix);
            return chunks;
        }

        if self.pending_paste_is_unterminated() {
            tracing::trace!(
                len = self.buffer.len(),
                "waiting for bracketed paste terminator"
            );
            return chunks;
        }

        if self.split_coalesced_escape
            && could_be_incomplete_doubled_escape_key_sequence(&self.buffer)
        {
            chunks.push(vec![ESC]);
            self.buffer.drain(..1);
        }

        if self.host_replies.awaiting_cell_size_or_appearance()
            && self.buffer.as_slice() == b"\x1b["
        {
            if !matches!(self.held, Held::HostReplyPrefix) && !mouse_wait_served {
                self.held = Held::HostReplyPrefix;
                tracing::trace!("holding incomplete host CSI reply one flush");
                return chunks;
            }
            self.host_replies.clear_cell_size_and_appearance();
            self.held = Held::Sequence;
        }

        if self.host_replies.awaiting_cell_size()
            && starts_with_incomplete_host_cell_size_report(&self.buffer)
        {
            tracing::debug!(
                len = self.buffer.len(),
                "discarding incomplete host cell size report after input timeout"
            );
            self.host_replies.clear_cell_size();
            self.begin_control_tail(ControlStringFamily::HostReplyCsi);
            return chunks;
        }

        if starts_with_incomplete_host_color_scheme_report(&self.buffer) {
            if self.host_replies.awaiting_appearance()
                && !matches!(self.held, Held::HostReplyPrefix)
            {
                self.held = Held::HostReplyPrefix;
                tracing::trace!(
                    len = self.buffer.len(),
                    "holding incomplete host color scheme report one flush"
                );
                return chunks;
            }
            tracing::debug!(
                len = self.buffer.len(),
                "discarding incomplete host color scheme report after input timeout"
            );
            self.host_replies.clear_appearance();
            self.begin_control_tail(ControlStringFamily::HostReplyCsi);
            return chunks;
        }

        if let Held::ControlString { family } = self.held {
            tracing::debug!(
                len = self.buffer.len(),
                "discarding incomplete host control string after input timeout"
            );
            // This intentionally gives host control replies precedence over legacy
            // Alt forms like Alt+] after timeout, so later reply tails cannot leak.
            self.begin_control_tail(family);
            return chunks;
        }

        if self.buffer.as_slice() == [ESC] {
            if self.host_replies.awaiting_reply()
                && !matches!(self.held, Held::HostReplyPrefix)
                && !mouse_wait_served
            {
                self.held = Held::HostReplyPrefix;
                tracing::trace!("holding lone escape one flush while awaiting host reply");
                return chunks;
            }
            // No continuation arrived; give up the window so Escape is not delayed again.
            self.host_replies.clear_all();
            tracing::warn!(
                len = self.buffer.len(),
                "flushing lone escape after input timeout; if this follows an alt chord or focus switch it may reach the pane as plain esc"
            );
            self.held = Held::EscapeReleased;
            chunks.push(std::mem::take(&mut self.buffer));
            return chunks;
        }

        if let Ok(text) = std::str::from_utf8(&self.buffer)
            && parse_terminal_key_sequence(text).is_some()
        {
            self.held = Held::None;
            chunks.push(std::mem::take(&mut self.buffer));
            return chunks;
        }

        // Buffer contents are user keystrokes; log lengths, never bytes.
        if starts_with_incomplete_utf8_char(&self.buffer) {
            tracing::trace!(
                len = self.buffer.len(),
                "waiting for UTF-8 continuation bytes"
            );
            self.held = Held::Utf8;
            return chunks;
        }

        if self.buffer.first() == Some(&ESC) && starts_with_incomplete_utf8_char(&self.buffer[1..])
        {
            tracing::trace!(
                len = self.buffer.len(),
                "waiting for escaped UTF-8 continuation bytes"
            );
            self.held = Held::Utf8;
            return chunks;
        }

        tracing::debug!(
            len = self.buffer.len(),
            "dropping incomplete raw input buffer after timeout"
        );
        self.held = Held::None;
        // `drain_available_chunks` consumed complete and malformed heads one
        // event at a time. Reaching this point means the only remaining bytes
        // are the incomplete trailing sequence, which idle timeout discards.
        self.buffer.clear();
        chunks
    }

    fn begin_control_tail(&mut self, family: ControlStringFamily) {
        let keep_st =
            family != ControlStringFamily::HostReplyCsi && self.buffer.last() == Some(&ESC);
        self.buffer.clear();
        if keep_st {
            self.buffer.push(ESC);
        }
        self.held = Held::ControlTail {
            family,
            bytes: 0,
            charged: self.buffer.len(),
        };
    }

    fn discard_oversized_csi_prefix(&mut self) {
        tracing::debug!(
            len = self.buffer.len(),
            max = MAX_INCOMPLETE_CSI_BYTES,
            "discarding oversized incomplete CSI sequence and its bounded tail"
        );
        self.begin_control_tail(ControlStringFamily::HostReplyCsi);
    }

    fn retain_timed_out_mouse_prefix(&mut self, prefix: Vec<u8>) {
        self.held = if prefix.len() < MAX_DISCARDED_CONTROL_TAIL_BYTES
            && plausible_sgr_mouse_prefix(&prefix)
        {
            Held::MouseTail { prefix }
        } else {
            Held::None
        };
    }

    fn drain_available_chunks(&mut self) -> Vec<Vec<u8>> {
        let mut chunks = Vec::new();

        loop {
            if matches!(self.held, Held::PasteTail { .. }) {
                if let Some(end) = find_subsequence(&self.buffer, BRACKETED_PASTE_END) {
                    self.buffer.drain(..end + BRACKETED_PASTE_END.len());
                    self.held = Held::None;
                    continue;
                }
                let keep = partial_suffix_len(&self.buffer, BRACKETED_PASTE_END);
                let drop = self.buffer.len() - keep;
                self.buffer.drain(..drop);
                break;
            }

            if let Held::MouseTail { prefix } = &self.held {
                match classify_sgr_mouse_continuation(prefix, &self.buffer) {
                    SgrMouseContinuation::Incomplete => break,
                    SgrMouseContinuation::Complete(len) => {
                        self.buffer.drain(..len);
                    }
                    SgrMouseContinuation::Invalid => {}
                }
                self.held = Held::None;
            }

            if matches!(self.held, Held::EscapeReleased) {
                if starts_with_incomplete_orphaned_sgr_mouse_tail(&self.buffer) {
                    break;
                }
                if discard_complete_orphaned_sgr_mouse_tail(&mut self.buffer) {
                    self.held = Held::None;
                    continue;
                }
                self.held = Held::None;
            }

            if let Held::ControlTail {
                family,
                bytes,
                charged,
            } = &mut self.held
            {
                if *family == ControlStringFamily::HostReplyCsi {
                    if discard_host_reply_csi_tail(&mut self.buffer, bytes) {
                        self.held = Held::None;
                        continue;
                    }
                    break;
                }
                let remaining = MAX_DISCARDED_CONTROL_TAIL_BYTES.saturating_sub(*bytes);
                let inspected = self.buffer.len().min(charged.saturating_add(remaining));
                if let Some(len) =
                    control_string_terminator_for_family(&self.buffer[..inspected], *family)
                {
                    self.buffer.drain(..len);
                    self.held = Held::None;
                    continue;
                }
                *bytes = bytes.saturating_add(inspected.saturating_sub(*charged));
                *charged = inspected;
                if *bytes >= MAX_DISCARDED_CONTROL_TAIL_BYTES {
                    self.buffer.drain(..inspected);
                    self.held = Held::None;
                    continue;
                }
                break;
            }

            if self.split_coalesced_escape
                && self.buffer.starts_with(b"\x1b\x1b")
                && !starts_with_complete_key_sequence(&self.buffer)
            {
                if could_be_incomplete_doubled_escape_key_sequence(&self.buffer) {
                    if self.buffer.len() >= MAX_INCOMPLETE_CSI_BYTES
                        && has_csi_introducer_after_escape_prefix(&self.buffer)
                    {
                        self.discard_oversized_csi_prefix();
                        continue;
                    }
                    self.held = Held::DoubledEscape;
                    break;
                }
                chunks.push(vec![ESC]);
                self.buffer.drain(..1);
                continue;
            }

            if self.pending_paste_is_unterminated() {
                if self.buffer.len() - BRACKETED_PASTE_START.len() > MAX_PENDING_PASTE_BYTES {
                    chunks.push(self.cut_oversized_paste());
                    continue;
                }
                break;
            }

            let Some((event, consumed)) = extract_one_event(&self.buffer) else {
                // An incomplete CSI remains whole in the buffer, so its length
                // is the cumulative byte count across reads. Complete frames
                // were extracted above before the bound is checked.
                if self.buffer.len() >= MAX_INCOMPLETE_CSI_BYTES
                    && has_csi_introducer_after_escape_prefix(&self.buffer)
                {
                    self.discard_oversized_csi_prefix();
                    continue;
                }
                if let Some(ControlString::Incomplete { family }) = control_string(&self.buffer) {
                    if self.buffer.len() >= MAX_DISCARDED_CONTROL_TAIL_BYTES {
                        let keep_st = self.buffer[MAX_DISCARDED_CONTROL_TAIL_BYTES - 1] == ESC;
                        self.buffer.drain(..MAX_DISCARDED_CONTROL_TAIL_BYTES);
                        if keep_st {
                            self.buffer.insert(0, ESC);
                        }
                        self.held = Held::ControlTail {
                            family,
                            bytes: 0,
                            charged: usize::from(keep_st),
                        };
                        continue;
                    }
                    self.held = Held::ControlString { family };
                } else if starts_with_incomplete_utf8_char(&self.buffer)
                    || self.buffer.first() == Some(&ESC)
                        && starts_with_incomplete_utf8_char(&self.buffer[1..])
                {
                    self.held = Held::Utf8;
                } else if self.has_pending_incomplete_mouse_sequence()
                    && !matches!(self.held, Held::MouseWait { .. })
                {
                    self.held = Held::MousePrefix;
                } else if !self.buffer.is_empty()
                    && !matches!(self.held, Held::HostReplyPrefix | Held::MouseWait { .. })
                {
                    self.held = Held::Sequence;
                } else if self.buffer.is_empty() && !matches!(self.held, Held::EscapeReleased) {
                    self.held = Held::None;
                }
                break;
            };
            self.host_replies.observe(&event);
            self.held = Held::None;
            chunks.push(self.buffer[..consumed].to_vec());
            self.buffer.drain(..consumed);
        }

        chunks
    }
}

fn plausible_control_string_tail(family: ControlStringFamily, buffer: &[u8]) -> bool {
    match family {
        ControlStringFamily::Osc => buffer.iter().all(|byte| {
            byte.is_ascii_digit()
                || matches!(
                    *byte,
                    b';' | b':'
                        | b'/'
                        | b'#'
                        | b'?'
                        | b'.'
                        | b'_'
                        | b'-'
                        | b'+'
                        | b'r'
                        | b'g'
                        | b'b'
                        | b'R'
                        | b'G'
                        | b'B'
                        | ESC
                )
        }),
        ControlStringFamily::StTerminated => buffer.last() == Some(&ESC),
        ControlStringFamily::HostReplyCsi => false,
    }
}

pub fn events_require_host_mode_refresh<'a>(
    events: impl IntoIterator<Item = &'a RawInputEvent>,
) -> bool {
    events
        .into_iter()
        .any(|event| matches!(event, RawInputEvent::OuterFocusGained))
}

fn extract_one_event(buffer: &[u8]) -> Option<(RawInputEvent, usize)> {
    if buffer.is_empty() {
        return None;
    }

    if buffer.starts_with(BRACKETED_PASTE_START) {
        let end = find_subsequence(buffer, BRACKETED_PASTE_END)?;
        // Decode lossily: a complete paste is always one event. Rejecting
        // invalid UTF-8 here would leave the framer stuck on it until the idle
        // flush dropped the paste and everything typed after it.
        let content = String::from_utf8_lossy(&buffer[BRACKETED_PASTE_START.len()..end]);
        return Some((
            RawInputEvent::Paste(content.into_owned()),
            end + BRACKETED_PASTE_END.len(),
        ));
    }

    if buffer[0] == ESC {
        if let Some(invalid_mouse_len) = malformed_double_escaped_sgr_mouse_len(buffer) {
            return Some((RawInputEvent::Unsupported, invalid_mouse_len));
        }

        let Some(seq_len) = complete_escape_sequence_len(buffer) else {
            // Keep valid partial escapes buffered, but let malformed UTF-8
            // Alt input and malformed CSI prefixes release the bytes behind
            // them. These are framed as one unsupported escape prefix so a
            // one-byte ESC chunk is not mistaken for a standalone Escape key.
            if let Some(invalid_len) = invalid_utf8_prefix_len(&buffer[1..]) {
                return Some((RawInputEvent::Unsupported, 1 + invalid_len));
            }
            if let Some(invalid_csi_len) = malformed_incomplete_csi_len(buffer) {
                return Some((RawInputEvent::Unsupported, invalid_csi_len));
            }
            // A doubled ESC wraps an inner sequence; when that inner sequence
            // is malformed, drop both escapes with it.
            if buffer.starts_with(b"\x1b\x1b")
                && let Some((RawInputEvent::Unsupported, inner_len)) =
                    extract_one_event(&buffer[1..])
            {
                return Some((RawInputEvent::Unsupported, 1 + inner_len));
            }
            return None;
        };
        if buffer[..seq_len].starts_with(b"\x1b[M") {
            let event = parse_default_mouse(&buffer[..seq_len])
                .map_or(RawInputEvent::Unsupported, RawInputEvent::Mouse);
            return Some((event, seq_len));
        }
        let Ok(seq) = std::str::from_utf8(&buffer[..seq_len]) else {
            // A completed escape sequence with invalid UTF-8 is one malformed
            // input event. Drop that sequence and continue with later input.
            return Some((RawInputEvent::Unsupported, seq_len));
        };

        if let Some((kind, color)) = parse_default_color_response(seq) {
            return Some((RawInputEvent::HostDefaultColor { kind, color }, seq_len));
        }
        if let Some((index, color)) = parse_palette_color_response(seq) {
            return Some((
                RawInputEvent::HostPaletteColors {
                    colors: vec![(index, color)],
                },
                seq_len,
            ));
        }

        match seq {
            "\x1b[I" => return Some((RawInputEvent::OuterFocusGained, seq_len)),
            "\x1b[O" => return Some((RawInputEvent::OuterFocusLost, seq_len)),
            _ => {}
        }

        if let Some(appearance) = parse_host_color_scheme_report(&buffer[..seq_len]) {
            return Some((RawInputEvent::HostColorSchemeChanged(appearance), seq_len));
        }

        if let Some((width_px, height_px)) = parse_host_cell_size_report(&buffer[..seq_len]) {
            return Some((
                RawInputEvent::HostCellSizeReport {
                    width_px,
                    height_px,
                },
                seq_len,
            ));
        }

        if let Some(mouse) = parse_sgr_mouse(seq) {
            return Some((RawInputEvent::Mouse(mouse), seq_len));
        }

        if let Some(key) = parse_terminal_key_sequence(seq) {
            return Some((RawInputEvent::Key(key), seq_len));
        }

        tracing::debug!(
            len = seq.len(),
            kind = "unsupported_escape_sequence",
            "dropping unsupported escape sequence"
        );
        return Some((RawInputEvent::Unsupported, seq_len));
    }

    let Some(consumed) = first_complete_utf8_char_len(buffer) else {
        return invalid_utf8_prefix_len(buffer).map(|_| (RawInputEvent::Unsupported, 1));
    };
    let Ok(text) = std::str::from_utf8(&buffer[..consumed]) else {
        return Some((RawInputEvent::Unsupported, 1));
    };
    let Some(key) = parse_terminal_key_sequence(text) else {
        return Some((RawInputEvent::Unsupported, consumed));
    };
    let key = key.with_text_commit();
    Some((RawInputEvent::Key(key), consumed))
}

/// Returns the malformed UTF-8 prefix length when the first character is
/// invalid, while leaving a valid but incomplete multibyte character pending.
fn invalid_utf8_prefix_len(buffer: &[u8]) -> Option<usize> {
    match std::str::from_utf8(buffer) {
        Err(error) if error.valid_up_to() == 0 => error.error_len(),
        _ => None,
    }
}

/// Returns the prefix length through an invalid byte in an unterminated CSI.
/// Bytes in a CSI before its final byte are ASCII parameter/intermediate bytes;
/// anything outside those ranges makes the sequence unrecoverable. The invalid
/// byte is consumed with the prefix: framed chunks are parsed again on their
/// own, so the chunk must still read as malformed without its successor.
fn malformed_incomplete_csi_len(buffer: &[u8]) -> Option<usize> {
    if !buffer.starts_with(b"\x1b[") {
        return None;
    }

    for (index, byte) in buffer.iter().enumerate().skip(2) {
        if (0x40..=0x7e).contains(byte) {
            return None;
        }
        if !(0x20..=0x3f).contains(byte) {
            return Some(index + 1);
        }
    }
    None
}

/// Returns the complete double-escaped SGR mouse frame when its payload is not
/// valid UTF-8. `complete_escape_sequence_len` uses the UTF-8 mouse parser for
/// this one disambiguation case and otherwise leaves such a frame pending.
fn malformed_double_escaped_sgr_mouse_len(buffer: &[u8]) -> Option<usize> {
    if !buffer.starts_with(b"\x1b\x1b[<") {
        return None;
    }

    let sequence_len = find_csi_final(&buffer[1..], b"Mm")?;
    let full_len = 1 + sequence_len;
    std::str::from_utf8(&buffer[1..full_len])
        .is_err()
        .then_some(full_len)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControlStringFamily {
    Osc,
    StTerminated,
    HostReplyCsi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[expect(
    variant_size_differences,
    reason = "a Copy scan result of sixteen bytes, returned by value from the input parser"
)]
enum ControlString {
    Complete {
        len: usize,
        family: ControlStringFamily,
    },
    Incomplete {
        family: ControlStringFamily,
    },
}

fn parse_host_color_scheme_report(buffer: &[u8]) -> Option<HostAppearance> {
    match buffer {
        GHOSTTY_COLOR_SCHEME_DARK_REPORT => Some(HostAppearance::Dark),
        GHOSTTY_COLOR_SCHEME_LIGHT_REPORT => Some(HostAppearance::Light),
        _ => None,
    }
}

/// Parses an XTWINOPS cell size report (`CSI 6 ; height ; width t`) into
/// `(width_px, height_px)`; note the reply orders height first.
fn parse_host_cell_size_report(buffer: &[u8]) -> Option<(u32, u32)> {
    let body = buffer.strip_prefix(b"\x1b[")?.strip_suffix(b"t")?;
    let text = std::str::from_utf8(body).ok()?;
    let mut params = text.split(';');
    if params.next()? != "6" {
        return None;
    }
    let height_px = params.next()?.parse::<u32>().ok()?;
    let width_px = params.next()?.parse::<u32>().ok()?;
    if params.next().is_some() {
        return None;
    }
    shepr_core::geometry::CellPx::new(width_px, height_px)
        .map(|cell| (cell.width.get(), cell.height.get()))
}

fn parse_host_keyboard_probe_response(buffer: &[u8]) -> Option<HostKeyboardProbeResponse> {
    let body = buffer.strip_prefix(b"\x1b[?")?;
    let (final_byte, parameters) = body.split_last()?;
    match final_byte {
        b'u' if !parameters.is_empty() && parameters.iter().all(u8::is_ascii_digit) => {
            let flags = std::str::from_utf8(parameters).ok()?.parse::<u16>().ok()?;
            Some(HostKeyboardProbeResponse::Flags(
                KittyKeyboardFlags::from_bits_retain(flags),
            ))
        }
        b'c' if !parameters.is_empty()
            && parameters
                .iter()
                .all(|byte| byte.is_ascii_digit() || *byte == b';') =>
        {
            Some(HostKeyboardProbeResponse::PrimaryDeviceAttributes)
        }
        _ => None,
    }
}

fn starts_with_incomplete_host_color_scheme_report(buffer: &[u8]) -> bool {
    buffer.starts_with(b"\x1b[?")
        && (GHOSTTY_COLOR_SCHEME_DARK_REPORT.starts_with(buffer)
            || GHOSTTY_COLOR_SCHEME_LIGHT_REPORT.starts_with(buffer))
        && buffer.len() < GHOSTTY_COLOR_SCHEME_DARK_REPORT.len()
}

fn starts_with_incomplete_host_cell_size_report(buffer: &[u8]) -> bool {
    let Some(body) = buffer.strip_prefix(b"\x1b[") else {
        return false;
    };
    if body.is_empty() || body.last() == Some(&b't') {
        return false;
    }

    let mut params = body.split(|byte| *byte == b';');
    if params.next() != Some(b"6".as_slice()) {
        return false;
    }
    let height = params.next();
    let width = params.next();
    params.next().is_none()
        && height.is_none_or(|value| value.iter().all(u8::is_ascii_digit))
        && width.is_none_or(|value| value.iter().all(u8::is_ascii_digit))
        && !(height.is_some_and(<[u8]>::is_empty) && width.is_some())
}

fn control_string(buffer: &[u8]) -> Option<ControlString> {
    let family = match buffer.get(..2)? {
        b"\x1b]" => ControlStringFamily::Osc,
        b"\x1bP" | b"\x1b_" | b"\x1b^" | b"\x1bX" => ControlStringFamily::StTerminated,
        _ => return None,
    };

    Some(match control_string_terminator_for_family(buffer, family) {
        Some(len) => ControlString::Complete { len, family },
        None => ControlString::Incomplete { family },
    })
}

fn first_complete_utf8_char_len(buffer: &[u8]) -> Option<usize> {
    let width = utf8_char_width(*buffer.first()?)?;

    if buffer.len() < width {
        return None;
    }

    std::str::from_utf8(&buffer[..width]).ok()?;
    Some(width)
}

fn starts_with_incomplete_utf8_char(buffer: &[u8]) -> bool {
    match std::str::from_utf8(buffer) {
        Ok(_) => false,
        Err(err) => err.valid_up_to() == 0 && err.error_len().is_none(),
    }
}

fn utf8_char_width(first: u8) -> Option<usize> {
    if first < 0x80 {
        Some(1)
    } else if first & 0b1110_0000 == 0b1100_0000 {
        Some(2)
    } else if first & 0b1111_0000 == 0b1110_0000 {
        Some(3)
    } else if first & 0b1111_1000 == 0b1111_0000 {
        Some(4)
    } else {
        None
    }
}

fn complete_escape_sequence_len(buffer: &[u8]) -> Option<usize> {
    if buffer.len() == 1 {
        return None;
    }

    if buffer.starts_with(b"\x1b\x1b[<")
        && let Some(mouse_len) = find_csi_final(&buffer[1..], b"Mm")
    {
        let mouse_sequence = std::str::from_utf8(&buffer[1..1 + mouse_len]).ok()?;
        if parse_sgr_mouse(mouse_sequence).is_some() {
            return Some(1);
        }
    }

    if buffer.len() >= 7
        && buffer.starts_with(b"\x1b\x1b[M")
        && parse_default_mouse(&buffer[1..7]).is_some()
    {
        return Some(1);
    }

    if buffer.starts_with(b"\x1b\x1b") {
        return complete_escape_sequence_len(&buffer[1..]).map(|len| len + 1);
    }

    if buffer.starts_with(b"\x1b[") {
        if buffer.starts_with(b"\x1b[<") {
            return find_csi_final(buffer, b"Mm");
        }
        if buffer.starts_with(b"\x1b[M") {
            return (buffer.len() >= 6).then_some(6);
        }
        return find_csi_final(
            buffer,
            b"@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~",
        );
    }

    if let Some(control) = control_string(buffer) {
        return match control {
            ControlString::Complete { len, .. } => Some(len),
            ControlString::Incomplete { .. } => None,
        };
    }

    if buffer.starts_with(b"\x1bO") {
        return (buffer.len() >= 3).then_some(3);
    }

    let escaped_char_width = utf8_char_width(buffer[1])?;
    if buffer.len() < 1 + escaped_char_width {
        return None;
    }
    std::str::from_utf8(&buffer[1..1 + escaped_char_width]).ok()?;
    Some(1 + escaped_char_width)
}

fn has_csi_introducer_after_escape_prefix(buffer: &[u8]) -> bool {
    let escape_count = buffer.iter().take_while(|byte| **byte == ESC).count();
    escape_count > 0 && buffer.get(escape_count) == Some(&b'[')
}

/// Keep a complete, parsed doubled-ESC key together for host input. Prefixes
/// that can still become a key wait for one idle flush; other ambiguous input
/// still separates ESC from a following key.
fn starts_with_complete_key_sequence(buffer: &[u8]) -> bool {
    let Some(sequence_len) = complete_escape_sequence_len(buffer) else {
        return false;
    };
    if sequence_len <= 1 {
        return false;
    }
    std::str::from_utf8(&buffer[..sequence_len])
        .ok()
        .and_then(parse_terminal_key_sequence)
        .is_some()
}

fn could_be_incomplete_doubled_escape_key_sequence(buffer: &[u8]) -> bool {
    if !buffer.starts_with(b"\x1b\x1b") {
        return false;
    }
    if buffer == b"\x1b\x1b" {
        return true;
    }

    let inner = &buffer[1..];
    if inner == b"\x1b[" || inner == b"\x1bO" {
        return true;
    }
    if inner.starts_with(b"\x1b[<") || !inner.starts_with(b"\x1b[") {
        return false;
    }

    inner[2..].iter().all(|byte| (0x20..=0x3f).contains(byte))
}

fn starts_with_incomplete_sgr_mouse_sequence(buffer: &[u8]) -> bool {
    buffer.starts_with(b"\x1b[<")
        && buffer[3..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b';')
}

/// Whether the bytes after a held `ESC` or `ESC [` still read as one escape
/// sequence (a mouse report, a host reply, a bracketed paste) rather than a key
/// followed by more input.
fn continues_escape_sequence(buffer: &[u8]) -> bool {
    match buffer {
        // OSC, DCS and APC host replies can also split after their ESC.
        [ESC] | [ESC, b'['] | [ESC, b']' | b'P' | b'_', ..] => true,
        [ESC, b'[', next, ..] => *next == b'M' || matches!(*next, 0x20..=0x3f),
        _ => false,
    }
}

/// Whether `buffer` is a prefix of, or starts, an SGR (`ESC [ <`) or default
/// (`ESC [ M`) mouse report.
fn could_continue_as_mouse_report(buffer: &[u8]) -> bool {
    [b"\x1b[<".as_slice(), b"\x1b[M"]
        .iter()
        .any(|prefix| buffer.starts_with(prefix) || prefix.starts_with(buffer))
}

fn starts_with_incomplete_default_mouse_sequence(buffer: &[u8]) -> bool {
    buffer.starts_with(b"\x1b[M") && buffer.len() < 6
}

fn starts_with_incomplete_orphaned_sgr_mouse_tail(buffer: &[u8]) -> bool {
    if buffer.len() > MAX_ORPHANED_SGR_MOUSE_TAIL_BYTES {
        return false;
    }
    buffer.len() < 3 && b"[<".starts_with(buffer)
        || buffer.starts_with(b"[<")
            && buffer[2..]
                .iter()
                .all(|byte| byte.is_ascii_digit() || *byte == b';')
}

fn discard_complete_orphaned_sgr_mouse_tail(buffer: &mut Vec<u8>) -> bool {
    let Some(terminator_len) = buffer
        .iter()
        .position(|byte| matches!(*byte, b'M' | b'm'))
        .map(|idx| idx + 1)
    else {
        return false;
    };
    if terminator_len > MAX_ORPHANED_SGR_MOUSE_TAIL_BYTES {
        return false;
    }
    let mut sequence = Vec::with_capacity(terminator_len + 1);
    sequence.push(ESC);
    sequence.extend_from_slice(&buffer[..terminator_len]);
    let Ok(sequence) = std::str::from_utf8(&sequence) else {
        return false;
    };
    if parse_sgr_mouse(sequence).is_none() {
        return false;
    }
    buffer.drain(..terminator_len);
    true
}

fn discard_host_reply_csi_tail(buffer: &mut Vec<u8>, discarded_tail_bytes: &mut usize) -> bool {
    let remaining = MAX_DISCARDED_CONTROL_TAIL_BYTES.saturating_sub(*discarded_tail_bytes);
    let inspected = buffer.len().min(remaining);

    for index in 0..inspected {
        match buffer[index] {
            0x20..=0x3f => {}
            0x40..=0x7e => {
                buffer.drain(..=index);
                return true;
            }
            _ => {
                buffer.drain(..index);
                return true;
            }
        }
    }

    buffer.drain(..inspected);
    *discarded_tail_bytes = discarded_tail_bytes.saturating_add(inspected);
    *discarded_tail_bytes >= MAX_DISCARDED_CONTROL_TAIL_BYTES
}

enum SgrMouseContinuation {
    Incomplete,
    Complete(usize),
    Invalid,
}

fn classify_sgr_mouse_continuation(prefix: &[u8], tail: &[u8]) -> SgrMouseContinuation {
    let remaining = MAX_DISCARDED_CONTROL_TAIL_BYTES.saturating_sub(prefix.len());
    let tail = &tail[..tail.len().min(remaining)];
    let final_index = tail
        .iter()
        .position(|byte| !byte.is_ascii_digit() && *byte != b';');
    let payload_len = final_index.unwrap_or(tail.len());
    let mut report = prefix.to_vec();
    report.extend_from_slice(&tail[..payload_len]);
    if !plausible_sgr_mouse_prefix(&report) {
        return SgrMouseContinuation::Invalid;
    }
    if let Some(index) = final_index {
        report.push(tail[index]);
        let valid = std::str::from_utf8(&report)
            .ok()
            .and_then(parse_sgr_mouse)
            .is_some();
        return if valid {
            SgrMouseContinuation::Complete(index + 1)
        } else {
            SgrMouseContinuation::Invalid
        };
    }
    if report.len() >= MAX_DISCARDED_CONTROL_TAIL_BYTES {
        SgrMouseContinuation::Invalid
    } else {
        SgrMouseContinuation::Incomplete
    }
}

// Reject impossible continuations early, without changing the general mouse
// parser. A partial last field (including zero) can still become valid.
fn plausible_sgr_mouse_prefix(report: &[u8]) -> bool {
    let Some(body) = report.strip_prefix(b"\x1b[<") else {
        return false;
    };
    let mut fields = body.split(|byte| *byte == b';').enumerate().peekable();
    while let Some((field, digits)) = fields.next() {
        if field > 2 {
            return false;
        }
        if digits.is_empty() {
            return fields.peek().is_none();
        }
        if !digits.iter().all(u8::is_ascii_digit) {
            return false;
        }
        let Some(value) = std::str::from_utf8(digits)
            .ok()
            .and_then(|digits| digits.parse::<u16>().ok())
        else {
            return false;
        };
        if field == 0 && value > u16::from(u8::MAX) {
            return false;
        }
        if fields.peek().is_some()
            && ((field == 0
                // `field == 0` was already bounds-checked above to be <= u8::MAX.
                && parse_mouse_cb(u8::try_from(value).unwrap_or(u8::MAX)).is_none())
                || (field == 1 && value == 0))
        {
            return false;
        }
    }
    true
}

fn osc_string_terminator(buffer: &[u8]) -> Option<usize> {
    let st = find_subsequence(buffer, b"\x1b\\").map(|idx| idx + 2);
    let bel = buffer
        .iter()
        .position(|byte| *byte == b'\x07')
        .map(|idx| idx + 1);

    match (st, bel) {
        (Some(st), Some(bel)) => Some(st.min(bel)),
        (Some(st), None) => Some(st),
        (None, Some(bel)) => Some(bel),
        (None, None) => None,
    }
}

fn st_string_terminator(buffer: &[u8]) -> Option<usize> {
    find_subsequence(buffer, b"\x1b\\").map(|idx| idx + 2)
}

fn control_string_terminator_for_family(
    buffer: &[u8],
    family: ControlStringFamily,
) -> Option<usize> {
    match family {
        ControlStringFamily::Osc => osc_string_terminator(buffer),
        ControlStringFamily::StTerminated => st_string_terminator(buffer),
        ControlStringFamily::HostReplyCsi => None,
    }
}

fn find_csi_final(buffer: &[u8], finals: &[u8]) -> Option<usize> {
    for (idx, byte) in buffer.iter().enumerate().skip(2) {
        if finals.contains(byte) {
            return Some(idx + 1);
        }
    }
    None
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_default_mouse(sequence: &[u8]) -> Option<MouseEvent> {
    let &[ESC, b'[', b'M', encoded_cb, encoded_column, encoded_row] = sequence else {
        return None;
    };
    let cb = encoded_cb.checked_sub(32)?;
    let column = u16::from(encoded_column).checked_sub(33)?;
    let row = u16::from(encoded_row).checked_sub(33)?;
    let (kind, modifiers) = parse_mouse_cb(cb)?;

    Some(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    })
}

fn parse_sgr_mouse(sequence: &str) -> Option<MouseEvent> {
    let body = sequence.strip_prefix("\x1b[<")?;
    let final_char = body.chars().last()?;
    if final_char != 'M' && final_char != 'm' {
        return None;
    }

    let payload = &body[..body.len() - 1];
    let mut parts = payload.split(';');
    let cb = parts.next()?.parse::<u8>().ok()?;
    let column = parts.next()?.parse::<u16>().ok()?.checked_sub(1)?;
    let row = parts.next()?.parse::<u16>().ok()?.checked_sub(1)?;
    let (kind, modifiers) = parse_mouse_cb(cb)?;

    let kind = if final_char == 'm' {
        match kind {
            MouseEventKind::Down(button) => MouseEventKind::Up(button),
            other => other,
        }
    } else {
        kind
    };

    Some(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    })
}

fn parse_mouse_cb(cb: u8) -> Option<(MouseEventKind, KeyModifiers)> {
    let button_number = (cb & MOUSE_BUTTON_FIELD_MASK)
        | ((cb & MOUSE_EXTENDED_BUTTON_FIELD_MASK) >> MOUSE_EXTENDED_BUTTON_SHIFT);
    let dragging = cb & MOUSE_DRAG_BIT != 0;

    let kind = match (
        mouse_button_from_code(button_number),
        button_number,
        dragging,
    ) {
        (Some(button), _, false) => MouseEventKind::Down(button),
        (Some(button), _, true) => MouseEventKind::Drag(button),
        (None, MOUSE_BUTTON_RELEASE, false) => MouseEventKind::Up(MouseButton::Left),
        (None, number, false) => mouse_scroll_from_code(number)?,
        // Crossterm cannot represent extended-button drags. Preserve their
        // position as motion so a stuck host button cannot suppress hover.
        (None, 3 | 4 | 5 | 8 | 9, true) => MouseEventKind::Moved,
        _ => return None,
    };
    Some((kind, mouse_modifiers_from_bits(cb)))
}

/// Parse raw terminal input bytes into a list of `RawInputEvent`s.
///
/// This directly extracts events without going through a channel, making it
/// suitable for synchronous use.
#[cfg(test)]
pub fn parse_raw_input_bytes_sync(data: &[u8]) -> Vec<RawInputEvent> {
    let mut framer = RawInputFramer::default();
    let mut events = framer.push(data);
    events.extend(framer.flush_timeout());
    events
}

#[cfg(test)]
impl RawInputFramer {
    pub fn push(&mut self, data: &[u8]) -> Vec<RawInputEvent> {
        self.push_framed(data)
            .into_iter()
            .map(|input| input.event)
            .collect()
    }

    pub fn flush_timeout(&mut self) -> Vec<RawInputEvent> {
        self.flush_timeout_framed()
            .into_iter()
            .map(|input| input.event)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEventKind};

    fn assert_raw_key(event: RawInputEvent, code: KeyCode, modifiers: KeyModifiers) {
        let RawInputEvent::Key(key) = event else {
            panic!("expected key");
        };
        assert_eq!(key.code, code);
        assert_eq!(key.modifiers, modifiers);
    }

    #[test]
    fn host_replies_keep_a_split_color_response_out_of_input() {
        let mut framer = RawInputFramer::for_host_input();
        framer.host_color_query_sent();

        assert!(framer.push_framed(b"\x1b").is_empty());
        assert!(framer.flush_timeout_framed().is_empty());

        let events = framer.push_framed(b"]11;rgb:2424/2727/3a3a\x1b\\");
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0].event,
            RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Background,
                ..
            }
        ));
    }

    fn decode_hex(hex: &str) -> Vec<u8> {
        let hex = hex.trim();
        assert_eq!(hex.len() % 2, 0, "hex string must have even length");
        (0..hex.len())
            .step_by(2)
            .map(|idx| u8::from_str_radix(&hex[idx..idx + 2], 16).expect("test precondition"))
            .collect()
    }

    fn parse_fixture_key_code(value: &str) -> KeyCode {
        match value {
            "enter" => KeyCode::Enter,
            "tab" => KeyCode::Tab,
            "backspace" => KeyCode::Backspace,
            "esc" => KeyCode::Esc,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" => KeyCode::PageUp,
            "pagedown" => KeyCode::PageDown,
            "insert" => KeyCode::Insert,
            "delete" => KeyCode::Delete,
            value if value.starts_with("char:") => KeyCode::Char(
                value
                    .trim_start_matches("char:")
                    .chars()
                    .next()
                    .expect("test precondition"),
            ),
            other => panic!("unsupported fixture key code: {other}"),
        }
    }

    fn parse_fixture_modifiers(value: &str) -> KeyModifiers {
        if value == "-" || value.is_empty() {
            return KeyModifiers::empty();
        }

        let mut modifiers = KeyModifiers::empty();
        for part in value.split('+') {
            match part {
                "shift" => modifiers |= KeyModifiers::SHIFT,
                "alt" => modifiers |= KeyModifiers::ALT,
                "control" => modifiers |= KeyModifiers::CONTROL,
                "super" => modifiers |= KeyModifiers::SUPER,
                "hyper" => modifiers |= KeyModifiers::HYPER,
                "meta" => modifiers |= KeyModifiers::META,
                other => panic!("unsupported fixture modifier: {other}"),
            }
        }
        modifiers
    }

    #[test]
    fn parses_kitty_shift_letter_release() {
        let (RawInputEvent::Key(key), consumed) =
            extract_one_event(b"\x1b[108:76;2:3u").expect("test precondition")
        else {
            panic!("expected key");
        };
        assert_eq!(consumed, 13);
        assert_eq!(key.code, KeyCode::Char('l'));
        assert_eq!(key.modifiers, KeyModifiers::SHIFT);
        assert_eq!(key.kind, KeyEventKind::Release);
        assert_eq!(key.shifted_codepoint, Some('L'));
    }

    #[test]
    fn parsed_keys_equal_their_synthesized_twins() {
        // Keys are semantic: the bytes a key was parsed from are not part of
        // its identity, so a parsed key equals one built from the same fields.
        let (RawInputEvent::Key(text), _) = extract_one_event(b"a").expect("test precondition")
        else {
            panic!("expected key");
        };
        assert_eq!(
            text,
            TerminalKey::new(KeyCode::Char('a'), KeyModifiers::empty()).with_text_commit()
        );

        let (RawInputEvent::Key(csi), _) =
            extract_one_event(b"\x1b[1;5A").expect("test precondition")
        else {
            panic!("expected key");
        };
        assert_eq!(csi, TerminalKey::new(KeyCode::Up, KeyModifiers::CONTROL));

        let mut framer = RawInputFramer::default();
        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.flush_timeout();
        let [RawInputEvent::Key(esc)] = events.as_slice() else {
            panic!("expected one key");
        };
        assert_eq!(esc, &TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()));
    }

    #[test]
    fn parses_bracketed_paste() {
        let (RawInputEvent::Paste(text), consumed) =
            extract_one_event(b"\x1b[200~hello\x1b[201~rest").expect("test precondition")
        else {
            panic!("expected paste");
        };
        assert_eq!(text, "hello");
        assert_eq!(consumed, 17);
    }

    #[test]
    fn parses_sgr_mouse() {
        let (RawInputEvent::Mouse(mouse), consumed) =
            extract_one_event(b"\x1b[<0;20;10M").expect("test precondition")
        else {
            panic!("expected mouse");
        };
        assert_eq!(consumed, 11);
        assert_eq!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
        assert_eq!(mouse.column, 19);
        assert_eq!(mouse.row, 9);
        assert_eq!(mouse.modifiers, KeyModifiers::empty());
    }

    #[test]
    fn parses_default_mouse_encoding() {
        let mut framer = RawInputFramer::default();
        let events = framer.push(b"\x1b[MCN1");
        let [RawInputEvent::Mouse(mouse)] = events.as_slice() else {
            panic!("expected one mouse event");
        };
        assert_eq!(mouse.kind, MouseEventKind::Moved);
        assert_eq!((mouse.column, mouse.row), (45, 16));
        assert_eq!(mouse.modifiers, KeyModifiers::empty());
    }

    #[test]
    fn rejected_default_mouse_frame_preserves_trailing_input() {
        let mut framer = RawInputFramer::default();
        let events = framer.push(b"\x1b[M\x82AAx");

        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], RawInputEvent::Unsupported));
        assert_raw_key(
            events.into_iter().nth(1).expect("test precondition"),
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn invalid_utf8_bytes_do_not_stall_later_keys() {
        for invalid_byte in [0x80, 0xf8] {
            let mut framer = RawInputFramer::default();
            let events = framer.push(&[invalid_byte, b'a', b'b']);

            assert_eq!(events.len(), 3, "invalid byte: {invalid_byte:#x}");
            assert!(matches!(&events[0], RawInputEvent::Unsupported));
            for (event, character) in events.into_iter().skip(1).zip(['a', 'b']) {
                assert_raw_key(event, KeyCode::Char(character), KeyModifiers::empty());
            }
            assert!(!framer.has_pending_input());
        }
    }

    #[test]
    fn invalid_utf8_after_escape_does_not_stall_later_keys() {
        let mut framer = RawInputFramer::default();

        let events = framer.push(b"\x1b\xffab");

        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], RawInputEvent::Unsupported));
        for (event, character) in events.into_iter().skip(1).zip(['a', 'b']) {
            assert_raw_key(event, KeyCode::Char(character), KeyModifiers::empty());
        }
        assert!(!framer.has_pending_input());
    }

    #[test]
    fn invalid_utf8_in_complete_csi_does_not_stall_later_keys() {
        let mut framer = RawInputFramer::default();

        let events = framer.push(b"\x1b[1\xffAab");

        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], RawInputEvent::Unsupported));
        for (event, character) in events.into_iter().skip(1).zip(['a', 'b']) {
            assert_raw_key(event, KeyCode::Char(character), KeyModifiers::empty());
        }
        assert!(!framer.has_pending_input());
    }

    #[test]
    fn invalid_utf8_in_incomplete_csi_does_not_stall_later_keys() {
        let mut framer = RawInputFramer::default();

        let events = framer.push(b"\x1b[1\xff12");

        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], RawInputEvent::Unsupported));
        for (event, character) in events.into_iter().skip(1).zip(['1', '2']) {
            assert_raw_key(event, KeyCode::Char(character), KeyModifiers::empty());
        }
        assert!(!framer.has_pending_input());
    }

    #[test]
    fn invalid_utf8_after_doubled_escape_does_not_stall_later_keys() {
        let mut framer = RawInputFramer::default();

        let events = framer.push(b"\x1b\x1b\xffab");

        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], RawInputEvent::Unsupported));
        for (event, character) in events.into_iter().skip(1).zip(['a', 'b']) {
            assert_raw_key(event, KeyCode::Char(character), KeyModifiers::empty());
        }
        assert!(!framer.has_pending_input());
    }

    #[test]
    fn invalid_utf8_in_coalesced_sgr_mouse_does_not_stall_later_keys() {
        let mut framer = RawInputFramer::default();

        let events = framer.push(b"\x1b\x1b[<0;\xff;10Mab");

        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], RawInputEvent::Unsupported));
        for (event, character) in events.into_iter().skip(1).zip(['a', 'b']) {
            assert_raw_key(event, KeyCode::Char(character), KeyModifiers::empty());
        }
        assert!(!framer.has_pending_input());
    }

    #[test]
    fn parses_extended_button_drag_as_mouse_motion() {
        for input in [
            b"\x1b[<160;20;10M".as_slice(),
            b"\x1b[<161;20;10M".as_slice(),
        ] {
            let (RawInputEvent::Mouse(mouse), _) =
                extract_one_event(input).expect("test precondition")
            else {
                panic!("expected mouse");
            };
            assert_eq!(mouse.kind, MouseEventKind::Moved);
            assert_eq!((mouse.column, mouse.row), (19, 9));
        }
    }

    #[test]
    fn parses_sgr_mouse_observable_modifiers() {
        let cases = [
            (b"\x1b[<8;20;10M".as_slice(), KeyModifiers::ALT),
            (b"\x1b[<16;20;10M".as_slice(), KeyModifiers::CONTROL),
            (
                b"\x1b[<24;20;10M".as_slice(),
                KeyModifiers::ALT | KeyModifiers::CONTROL,
            ),
        ];

        for (input, expected) in cases {
            let (RawInputEvent::Mouse(mouse), _) =
                extract_one_event(input).expect("test precondition")
            else {
                panic!("expected mouse");
            };
            assert_eq!(mouse.modifiers, expected);
            assert!(!mouse.modifiers.contains(KeyModifiers::SUPER));
        }
    }

    #[test]
    fn parses_host_default_color_response_with_st() {
        let (RawInputEvent::HostDefaultColor { kind, color }, consumed) =
            extract_one_event(b"\x1b]10;rgb:cccc/dddd/eeee\x1b\\").expect("test precondition")
        else {
            panic!("expected host color response");
        };
        assert_eq!(consumed, 25);
        assert_eq!(kind, DefaultColorKind::Foreground);
        assert_eq!(
            color,
            RgbColor {
                r: 0xcc,
                g: 0xdd,
                b: 0xee
            }
        );
    }

    #[test]
    fn parses_host_default_color_response_with_bel() {
        let (RawInputEvent::HostDefaultColor { kind, color }, consumed) =
            extract_one_event(b"\x1b]11;#112233\x07").expect("test precondition")
        else {
            panic!("expected host color response");
        };
        assert_eq!(consumed, 13);
        assert_eq!(kind, DefaultColorKind::Background);
        assert_eq!(
            color,
            RgbColor {
                r: 0x11,
                g: 0x22,
                b: 0x33
            }
        );
    }

    #[test]
    fn parses_host_palette_color_response() {
        let (RawInputEvent::HostPaletteColors { colors }, consumed) =
            extract_one_event(b"\x1b]4;7;rgb:1111/2222/3333\x1b\\").expect("test precondition")
        else {
            panic!("expected host palette response");
        };
        assert_eq!(consumed, 26);
        assert_eq!(
            colors,
            vec![(
                7,
                RgbColor {
                    r: 0x11,
                    g: 0x22,
                    b: 0x33,
                }
            )]
        );
    }

    #[test]
    fn parses_legacy_up_arrow() {
        let (RawInputEvent::Key(key), consumed) =
            extract_one_event(b"\x1b[A").expect("test precondition")
        else {
            panic!("expected key");
        };
        assert_eq!(consumed, 3);
        assert_eq!(key.code, KeyCode::Up);
    }

    #[test]
    fn parses_outer_focus_events() {
        let (event, consumed) = extract_one_event(b"\x1b[I").expect("test precondition");
        assert_eq!(consumed, 3);
        assert!(matches!(event, RawInputEvent::OuterFocusGained));

        let (event, consumed) = extract_one_event(b"\x1b[O").expect("test precondition");
        assert_eq!(consumed, 3);
        assert!(matches!(event, RawInputEvent::OuterFocusLost));
    }

    #[test]
    fn outer_focus_gained_requests_host_mode_refresh() {
        assert!(events_require_host_mode_refresh(
            &parse_raw_input_bytes_sync(b"\x1b[I")
        ));
        assert!(!events_require_host_mode_refresh(
            &parse_raw_input_bytes_sync(b"\x1b[O")
        ));
    }

    #[test]
    fn parses_ghostty_color_scheme_reports() {
        for bytes in [
            GHOSTTY_COLOR_SCHEME_DARK_REPORT,
            GHOSTTY_COLOR_SCHEME_LIGHT_REPORT,
        ] {
            let events = parse_raw_input_bytes_sync(bytes);
            assert_eq!(events.len(), 1, "bytes: {bytes:?}");
            assert!(matches!(
                events[0],
                RawInputEvent::HostColorSchemeChanged(HostAppearance::Dark | HostAppearance::Light)
            ));
        }
    }

    #[test]
    fn ghostty_color_scheme_report_parser_is_exact() {
        for bytes in [
            b"\x1b[?997;0n".as_slice(),
            b"\x1b[?997;3n".as_slice(),
            b"\x1b[?998;1n".as_slice(),
        ] {
            let events = parse_raw_input_bytes_sync(bytes);
            assert_eq!(events.len(), 1, "bytes: {bytes:?}");
            assert!(matches!(events[0], RawInputEvent::Unsupported));
        }
    }

    #[test]
    fn raw_input_framer_reassembles_split_color_scheme_report() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b[?997;").is_empty());
        let events = framer.push(b"1n");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            RawInputEvent::HostColorSchemeChanged(HostAppearance::Dark)
        ));
    }

    #[test]
    fn parses_host_cell_size_report() {
        let events = parse_raw_input_bytes_sync(b"\x1b[6;21;10t");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            RawInputEvent::HostCellSizeReport {
                width_px: 10,
                height_px: 21,
            }
        ));
    }

    #[test]
    fn host_cell_size_report_parser_is_exact() {
        for bytes in [
            // Zero dimensions carry no usable cell size.
            b"\x1b[6;0;10t".as_slice(),
            b"\x1b[6;21;0t".as_slice(),
            // Missing or extra parameters.
            b"\x1b[6;21t".as_slice(),
            b"\x1b[6;21;10;3t".as_slice(),
            // Other XTWINOPS reports must not be mistaken for a cell size.
            b"\x1b[4;1610;777t".as_slice(),
            b"\x1b[8;37;161t".as_slice(),
            // Non-numeric parameters.
            b"\x1b[6;21;1-t".as_slice(),
        ] {
            assert!(
                parse_host_cell_size_report(bytes).is_none(),
                "bytes: {bytes:?}"
            );
        }
    }

    #[test]
    fn split_color_scheme_timeout_does_not_swallow_legacy_alt_bracket() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b[").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b[".to_vec()]);
    }

    #[test]
    fn raw_input_byte_framer_discards_timed_out_split_color_scheme_report_tail() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b[?997;").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"1n").is_empty());
        assert_eq!(framer.push(b"a"), vec![b"a".to_vec()]);
        assert!(framer.flush_timeout().is_empty());
    }

    #[test]
    fn parses_xterm_alt_up_arrow() {
        let (RawInputEvent::Key(key), consumed) =
            extract_one_event(b"\x1b[1;3A").expect("test precondition")
        else {
            panic!("expected key");
        };
        assert_eq!(consumed, 6);
        assert_eq!(key.code, KeyCode::Up);
        assert_eq!(key.modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn parses_legacy_alt_backspace() {
        let (RawInputEvent::Key(key), consumed) =
            extract_one_event(b"\x1b\x7f").expect("test precondition")
        else {
            panic!("expected key");
        };
        assert_eq!(consumed, 2);
        assert_eq!(key.code, KeyCode::Backspace);
        assert_eq!(key.modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn parses_kitty_alt_backspace() {
        let (RawInputEvent::Key(key), consumed) =
            extract_one_event(b"\x1b[127;3u").expect("test precondition")
        else {
            panic!("expected key");
        };
        assert_eq!(consumed, 8);
        assert_eq!(key.code, KeyCode::Backspace);
        assert_eq!(key.modifiers, KeyModifiers::ALT);
    }

    #[test]
    fn parses_enhanced_pageup_press() {
        let (RawInputEvent::Key(key), consumed) =
            extract_one_event(b"\x1b[5;1:1~").expect("test precondition")
        else {
            panic!("expected key");
        };
        assert_eq!(consumed, 8);
        assert_eq!(key.code, KeyCode::PageUp);
        assert_eq!(key.modifiers, KeyModifiers::empty());
        assert_eq!(key.kind, KeyEventKind::Press);
    }

    #[test]
    fn parses_enhanced_pagedown_release() {
        let (RawInputEvent::Key(key), consumed) =
            extract_one_event(b"\x1b[6;1:3~").expect("test precondition")
        else {
            panic!("expected key");
        };
        assert_eq!(consumed, 8);
        assert_eq!(key.code, KeyCode::PageDown);
        assert_eq!(key.modifiers, KeyModifiers::empty());
        assert_eq!(key.kind, KeyEventKind::Release);
    }

    #[test]
    fn raw_input_family_matrix_is_covered() {
        let cases: &[(&[u8], KeyCode, KeyModifiers)] = &[
            (b"\x02", KeyCode::Char('b'), KeyModifiers::CONTROL),
            (b"\r", KeyCode::Enter, KeyModifiers::empty()),
            (b"\t", KeyCode::Tab, KeyModifiers::empty()),
            (b"\x7f", KeyCode::Backspace, KeyModifiers::empty()),
            (b"\x1b[A", KeyCode::Up, KeyModifiers::empty()),
            (b"\x1b[1;3A", KeyCode::Up, KeyModifiers::ALT),
            (b"\x1b\x7f", KeyCode::Backspace, KeyModifiers::ALT),
            (b"\x1b[127;3u", KeyCode::Backspace, KeyModifiers::ALT),
            (b"\x1b[57420;1u", KeyCode::Down, KeyModifiers::empty()),
            (b"\x1b[57423;1u", KeyCode::Home, KeyModifiers::empty()),
            (b"\x1bOq", KeyCode::Char('1'), KeyModifiers::empty()),
            (b"\x1b[14~", KeyCode::F(4), KeyModifiers::empty()),
            (b"\x1b[11;2~", KeyCode::F(1), KeyModifiers::SHIFT),
            (b"\x1b[13;1:1~", KeyCode::F(3), KeyModifiers::empty()),
            (b"\x1b[14;3~", KeyCode::F(4), KeyModifiers::ALT),
            (b"\x1b[57364;1u", KeyCode::F(1), KeyModifiers::empty()),
            (b"\x1b[57366;1u", KeyCode::F(3), KeyModifiers::empty()),
            (b"\x1b[57366;2u", KeyCode::F(3), KeyModifiers::SHIFT),
            (b"\x1b[57375;1u", KeyCode::F(12), KeyModifiers::empty()),
            (b"\x1b[57376;1u", KeyCode::F(13), KeyModifiers::empty()),
            (b"\x1b[49:33;2:1u", KeyCode::Char('1'), KeyModifiers::SHIFT),
        ];

        for (bytes, code, modifiers) in cases {
            let (event, consumed) = extract_one_event(bytes).expect("test precondition");
            assert_eq!(consumed, bytes.len());
            assert_raw_key(event, *code, *modifiers);
        }
    }

    #[test]
    fn raw_framer_waits_for_application_keypad_sequence_final_byte() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1bO").is_empty());
        let events = framer.push(b"q");

        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().expect("test precondition"),
            KeyCode::Char('1'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn unsupported_ss3_sequence_stays_unsupported() {
        let (event, consumed) = extract_one_event(b"\x1bOz").expect("test precondition");

        assert_eq!(consumed, 3);
        assert!(matches!(event, RawInputEvent::Unsupported));
    }

    #[test]
    fn parses_modified_rxvt_f_key_alias() {
        let (event, consumed) = extract_one_event(b"\x1b[14;3~").expect("test precondition");

        assert_eq!(consumed, 7);
        assert_raw_key(event, KeyCode::F(4), KeyModifiers::ALT);
    }

    #[test]
    fn flushes_lone_escape_after_timeout() {
        let mut framer = RawInputFramer::default();
        assert!(framer.push(&[ESC]).is_empty());

        let events = framer.flush_timeout();
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().expect("test precondition"),
            KeyCode::Esc,
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn parses_raw_ctrl_b() {
        let (RawInputEvent::Key(key), consumed) =
            extract_one_event(b"\x02").expect("test precondition")
        else {
            panic!("expected key");
        };
        assert_eq!(consumed, 1);
        assert_eq!(key.code, KeyCode::Char('b'));
        assert_eq!(key.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn parses_raw_lf_as_ctrl_j() {
        let (RawInputEvent::Key(key), consumed) =
            extract_one_event(b"\n").expect("test precondition")
        else {
            panic!("expected key");
        };
        assert_eq!(consumed, 1);
        assert_eq!(key.code, KeyCode::Char('j'));
        assert_eq!(key.modifiers, KeyModifiers::CONTROL);
    }

    fn assert_fixture_extracts_whole_events(corpus: &str) {
        for line in corpus.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let mut columns: Vec<_> = line.split('\t').collect();
            if columns.len() == 5 {
                columns.push("");
            }
            let (bytes_hex, code, modifiers) = match columns.len() {
                6 => {
                    if columns[1].chars().all(|ch| ch.is_ascii_hexdigit()) {
                        (columns[1], columns[2], columns[3])
                    } else {
                        (columns[2], columns[3], columns[4])
                    }
                }
                7 => (columns[2], columns[3], columns[4]),
                _ => panic!("fixture row must have 6 or 7 columns: {line}"),
            };
            assert!(
                bytes_hex.chars().all(|ch| ch.is_ascii_hexdigit()),
                "non-hex fixture bytes: {bytes_hex} in {line}"
            );
            let bytes = decode_hex(bytes_hex);
            let (event, consumed) = extract_one_event(&bytes).expect("test precondition");
            assert_eq!(
                consumed,
                bytes.len(),
                "fixture should extract a whole event: {line}"
            );
            assert_raw_key(
                event,
                parse_fixture_key_code(code),
                parse_fixture_modifiers(modifiers),
            );
        }
    }

    fn read_fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
    }

    #[test]
    fn raw_input_corpus_fixture_extracts_whole_events() {
        assert_fixture_extracts_whole_events(&read_fixture("keyboard_protocol_corpus.tsv"));
    }

    #[test]
    fn raw_input_linux_terminal_variants_fixture_extracts_whole_events() {
        assert_fixture_extracts_whole_events(&read_fixture("linux_terminal_variants.tsv"));
    }

    #[test]
    fn chunked_legacy_arrow_waits_for_completion() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.push(b"[A");
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().expect("test precondition"),
            KeyCode::Up,
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn lone_escape_is_buffered_until_timeout_flush() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.flush_timeout();
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().expect("test precondition"),
            KeyCode::Esc,
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn escape_followed_by_arrow_before_flush_does_not_emit_escape() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.push(b"[B");
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().expect("test precondition"),
            KeyCode::Down,
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn escape_followed_by_sgr_mouse_before_flush_does_not_emit_text() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.push(b"[<65;43;26M");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 42,
                row: 25,
                ..
            })
        ));
    }

    #[test]
    fn lone_escape_then_complete_sgr_mouse_report_emits_both_events() {
        for report in [b"\x1b[<35;10;20M".as_slice(), b"\x1b[<35;10;20m".as_slice()] {
            let mut framer = RawInputFramer::default();

            assert!(framer.push(b"\x1b").is_empty());
            let events = framer.push(report);

            assert_eq!(events.len(), 2);
            let mut events = events.into_iter();
            assert_raw_key(
                events.next().expect("test precondition"),
                KeyCode::Esc,
                KeyModifiers::empty(),
            );
            assert!(matches!(
                events.next().expect("test precondition"),
                RawInputEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: 9,
                    row: 19,
                    ..
                })
            ));
            assert!(framer.flush_timeout().is_empty());
        }
    }

    #[test]
    fn lone_escape_then_default_mouse_report_emits_both_events() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.push(b"\x1b[MCN1");

        assert_eq!(events.len(), 2);
        let mut events = events.into_iter();
        assert_raw_key(
            events.next().expect("test precondition"),
            KeyCode::Esc,
            KeyModifiers::empty(),
        );
        assert!(matches!(
            events.next().expect("test precondition"),
            RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 45,
                row: 16,
                ..
            })
        ));
        assert!(framer.flush_timeout().is_empty());
    }

    #[test]
    fn legacy_doubled_escape_alt_arrow_remains_one_event() {
        let mut framer = RawInputFramer::default();

        let events = framer.push(b"\x1b\x1b[A");

        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().expect("test precondition"),
            KeyCode::Up,
            KeyModifiers::ALT,
        );
        assert!(framer.flush_timeout().is_empty());
    }

    #[test]
    fn host_input_keeps_complete_legacy_alt_arrow_together() {
        let mut framer = RawInputFramer::for_host_input();

        let events = framer.push(b"\x1b\x1b[D");

        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().expect("test precondition"),
            KeyCode::Left,
            KeyModifiers::ALT,
        );
    }

    #[test]
    fn host_input_reassembles_legacy_alt_arrow_split_across_reads() {
        for (prefix, tail) in [
            (b"\x1b\x1b".as_slice(), b"[A".as_slice()),
            (b"\x1b\x1b[".as_slice(), b"A".as_slice()),
        ] {
            let mut framer = RawInputFramer::for_host_input();

            assert!(framer.push(prefix).is_empty());
            let events = framer.push(tail);

            assert_eq!(events.len(), 1);
            assert_raw_key(
                events.into_iter().next().expect("test precondition"),
                KeyCode::Up,
                KeyModifiers::ALT,
            );
            assert!(framer.flush_timeout().is_empty());
        }
    }

    #[test]
    fn host_input_still_splits_unrecognized_doubled_escape() {
        let mut framer = RawInputByteFramer::for_host_input();

        assert_eq!(
            framer.push(b"\x1b\x1bx"),
            vec![b"\x1b".to_vec(), b"\x1bx".to_vec()]
        );
    }

    #[test]
    fn host_input_flushes_uncompleted_doubled_escape_at_idle_timeout() {
        let mut framer = RawInputByteFramer::for_host_input();

        assert!(framer.push(b"\x1b\x1b").is_empty());
        assert_eq!(
            framer.flush_timeout(),
            vec![b"\x1b".to_vec(), b"\x1b".to_vec()]
        );
    }

    #[test]
    fn host_input_double_escape_uses_one_idle_window_to_disambiguate() {
        let mut split_alt_arrow = RawInputFramer::for_host_input();
        assert!(split_alt_arrow.push(b"\x1b\x1b").is_empty());
        assert_eq!(
            split_alt_arrow.held_input_flush_timeout_ms(),
            RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS
        );
        let events = split_alt_arrow.push(b"[A");
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().expect("test precondition"),
            KeyCode::Up,
            KeyModifiers::ALT,
        );

        let mut two_escapes = RawInputFramer::for_host_input();
        assert!(two_escapes.push(b"\x1b\x1b").is_empty());
        assert_eq!(
            two_escapes.held_input_flush_timeout_ms(),
            RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS
        );
        let events = two_escapes.flush_timeout();
        assert_eq!(events.len(), 2);
        for event in events {
            assert_raw_key(event, KeyCode::Esc, KeyModifiers::empty());
        }
    }

    #[test]
    fn default_framer_preserves_doubled_escape_alt_arrow() {
        let mut framer = RawInputByteFramer::default();

        assert_eq!(framer.push(b"\x1b\x1b[D"), vec![b"\x1b\x1b[D".to_vec()]);
    }

    #[test]
    fn sgr_mouse_sequence_split_after_button_prefix_is_reassembled_before_timeout() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b[<3").is_empty());
        let events = framer.push(b"5;58;30M");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            RawInputEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: 57,
                row: 29,
                ..
            })
        ));
    }

    #[test]
    fn timed_out_split_sgr_mouse_tail_is_discarded_and_following_input_is_preserved() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b[<3").is_empty());
        assert!(framer.flush_timeout().is_empty());
        let events = framer.push(b"5;58;30Mx");

        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().expect("test precondition"),
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn captured_sgr_mouse_tail_after_second_idle_flush_is_discarded() {
        let mut framer = RawInputByteFramer::for_host_input();

        // A captured host sequence: this prefix timed out, then its tail
        // arrived 33 ms later. The Unix reader flushes again
        // after 10 ms of continued idle following the first discard.
        assert!(framer.push(b"\x1b[<3").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.push(b"5;28;31M"), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn timed_out_sgr_mouse_invalid_completion_is_preserved_after_idle() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b[<3").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.push(b"M"), vec![b"M".to_vec()]);
    }

    #[test]
    fn timed_out_sgr_mouse_completion_survives_read_splits_and_idle() {
        let tail = b"5;28;31M";
        for split in 0..=tail.len() {
            let mut framer = RawInputByteFramer::for_host_input();
            assert!(framer.push(b"\x1b[<3").is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert!(framer.push(&tail[..split]).is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert!(framer.flush_timeout().is_empty());
            let mut rest = tail[split..].to_vec();
            rest.extend_from_slice(b"x\x1b[A");
            assert_eq!(framer.push(&rest), vec![b"x".to_vec(), b"\x1b[A".to_vec()]);
            assert!(!matches!(framer.held, Held::MouseTail { .. }));
            assert!(!framer.has_pending_input());
        }
    }

    #[test]
    fn timed_out_sgr_mouse_invalid_syntax_releases_continuation() {
        for tail in [
            b"5;0;31M".as_slice(), // zero coordinate
            b"5;;31M",             // empty field
            b"5;28;31;1M",         // extra field (the general parser is permissive)
            b"999;28;31M",         // button overflow
            b"5;65536;31M",        // coordinate overflow
        ] {
            let mut framer = RawInputByteFramer::default();
            assert!(framer.push(b"\x1b[<3").is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert_eq!(framer.push(tail).concat(), tail);
            assert!(!matches!(framer.held, Held::MouseTail { .. }));
        }
    }

    #[test]
    fn timed_out_sgr_mouse_interruption_preserves_text_and_new_events() {
        for suffix in [
            b"x".as_slice(),
            b"\x1b[A",                  // new key sequence
            b"\x1b[200~paste\x1b[201~", // bracketed paste
            "\u{4f60}".as_bytes(),      // UTF-8 split across reads
        ] {
            let mut framer = RawInputByteFramer::default();
            assert!(framer.push(b"\x1b[<3").is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert!(framer.push(b"5;28;").is_empty());
            assert!(framer.flush_timeout().is_empty());
            let mut chunks = Vec::new();
            for byte in suffix {
                chunks.extend(framer.push(&[*byte]));
            }
            let mut expected = b"5;28;".to_vec();
            expected.extend_from_slice(suffix);
            assert_eq!(chunks.concat(), expected);
            assert!(!matches!(framer.held, Held::MouseTail { .. }));
            assert_eq!(framer.push(b"123M").concat(), b"123M");
        }
    }

    #[test]
    fn timed_out_sgr_mouse_budget_includes_prefix_and_preserves_overflow() {
        let prefix = b"\x1b[<35;1;";
        let remaining = MAX_DISCARDED_CONTROL_TAIL_BYTES - prefix.len();
        let mut framer = RawInputByteFramer::default();
        assert!(framer.push(prefix).is_empty());
        assert!(framer.flush_timeout().is_empty());
        let mut valid_tail = vec![b'0'; remaining - 2];
        valid_tail.extend_from_slice(b"1M");
        assert!(framer.push(&valid_tail).is_empty()); // complete exactly at limit
        assert!(!matches!(framer.held, Held::MouseTail { .. }));

        for length in [remaining, remaining + 1024] {
            let mut framer = RawInputByteFramer::default();
            assert!(framer.push(prefix).is_empty());
            assert!(framer.flush_timeout().is_empty());
            let tail = vec![b'0'; length];
            assert_eq!(framer.push(&tail).concat(), tail);
            assert!(!matches!(framer.held, Held::MouseTail { .. }));
            assert_eq!(framer.push(b"1Mtext").concat(), b"1Mtext");
        }

        let mut framer = RawInputByteFramer::default();
        assert!(framer.push(prefix).is_empty());
        assert!(framer.flush_timeout().is_empty());
        let mut tail = vec![b'0'; remaining - 1];
        assert!(framer.push(&tail).is_empty());
        assert!(framer.flush_timeout().is_empty());
        tail.extend_from_slice(b"01M");
        assert_eq!(framer.push(b"01M").concat(), tail);
        assert!(!matches!(framer.held, Held::MouseTail { .. }));
    }

    #[test]
    fn idle_flush_releases_a_lone_csi_or_ss3_introducer_as_its_alt_key() {
        // A lone `ESC [` or `ESC O` is not a complete escape sequence, so the
        // escape framing cannot read it; after the idle flush it is Alt+[ or
        // Alt+O, not dropped.
        for (bytes, ch) in [(b"\x1b[".as_slice(), '['), (b"\x1bO", 'O')] {
            let mut framer = RawInputFramer::default();
            assert!(framer.push(bytes).is_empty());

            let events = framer.flush_timeout();
            assert_eq!(events.len(), 1, "{bytes:?}");
            let expected = if ch.is_ascii_uppercase() {
                KeyModifiers::ALT | KeyModifiers::SHIFT
            } else {
                KeyModifiers::ALT
            };
            assert_raw_key(
                events.into_iter().next().expect("test precondition"),
                KeyCode::Char(ch),
                expected,
            );
        }
    }

    #[test]
    fn confirmed_host_disambiguation_keeps_kitty_escape_immediate() {
        let mut framer = RawInputFramer::default();
        framer
            .byte_framer
            .set_host_escape_disambiguation_active(true);

        let events = framer.push(b"\x1b[27u");

        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().expect("test precondition"),
            KeyCode::Esc,
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn sgr_mouse_tail_after_lone_escape_timeout_is_discarded() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let timeout_events = framer.flush_timeout();
        assert_eq!(timeout_events.len(), 1);
        assert_raw_key(
            timeout_events
                .into_iter()
                .next()
                .expect("test precondition"),
            KeyCode::Esc,
            KeyModifiers::empty(),
        );

        assert!(framer.push(b"[<65;43;26M").is_empty());
    }

    #[test]
    fn input_after_discarded_complete_sgr_mouse_tail_is_preserved() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout().len(), 1);
        let events = framer.push(b"[<65;43;26Mx");
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().expect("test precondition"),
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn invalid_orphaned_sgr_mouse_tail_after_escape_timeout_is_preserved() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout().len(), 1);

        let events = framer.push(b"[<x");

        assert_eq!(events.len(), 3);
        assert_raw_key(
            events.into_iter().last().expect("test precondition"),
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn double_split_sgr_mouse_tail_after_lone_escape_timeout_is_discarded() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout().len(), 1);

        assert!(framer.push(b"[<65;4").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.flush_timeout().is_empty());
        let events = framer.push(b"3;26Mx");
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().expect("test precondition"),
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn escape_followed_by_alt_char_before_flush_becomes_alt_key() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        let events = framer.push(b"b");
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().expect("test precondition"),
            KeyCode::Char('b'),
            KeyModifiers::ALT,
        );
    }

    #[test]
    fn chunked_kitty_sequence_waits_for_completion() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b[49:33;2:").is_empty());
        let events = framer.push(b"1u");
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().expect("test precondition"),
            KeyCode::Char('1'),
            KeyModifiers::SHIFT,
        );
    }

    #[test]
    fn chunked_bracketed_paste_waits_for_terminator() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b[200~hello").is_empty());
        let events = framer.push(b"\x1b[201~");
        assert_eq!(events.len(), 1);
        let RawInputEvent::Paste(text) = &events[0] else {
            panic!("expected paste");
        };
        assert_eq!(text, "hello");
    }

    #[test]
    fn incomplete_bracketed_paste_is_not_flushed_on_timeout() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b[200~hello\nworld").is_empty());
        assert!(framer.flush_timeout().is_empty());
        let events = framer.push(b"\x1b[201~");
        assert_eq!(events.len(), 1);
        let RawInputEvent::Paste(text) = &events[0] else {
            panic!("expected paste");
        };
        assert_eq!(text, "hello\nworld");
    }

    #[test]
    fn paste_with_invalid_utf8_is_delivered_and_does_not_swallow_later_keys() {
        let mut framer = RawInputFramer::default();

        let events = framer.push(b"\x1b[200~a\xffb\x1b[201~x");

        assert_eq!(events.len(), 2);
        let mut events = events.into_iter();
        let Some(RawInputEvent::Paste(text)) = events.next() else {
            panic!("expected paste");
        };
        assert_eq!(text, "a\u{FFFD}b");
        assert_raw_key(
            events.next().expect("test precondition"),
            KeyCode::Char('x'),
            KeyModifiers::empty(),
        );
        assert!(framer.flush_timeout().is_empty());
    }

    #[test]
    fn host_framer_forwards_invalid_utf8_paste_bytes_unchanged() {
        let mut framer = RawInputByteFramer::for_host_input();

        assert_eq!(
            framer.push(b"\x1b[200~a\xffb\x1b[201~x"),
            vec![b"\x1b[200~a\xffb\x1b[201~".to_vec(), b"x".to_vec()]
        );
        assert!(!framer.has_pending_input());
    }

    #[test]
    fn stalled_unterminated_paste_is_delivered_when_input_resumes() {
        let mut framer = RawInputByteFramer::for_host_input();
        let start = std::time::Instant::now();

        assert!(framer.push_at(b"\x1b[200~hello", start).is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.has_pending_input());

        // Nothing more of the paste arrives; the user types a key.
        let chunks = framer.push_at(b"x", start + PASTE_STALL_TIMEOUT);

        assert_eq!(
            chunks,
            vec![b"\x1b[200~hello\x1b[201~".to_vec(), b"x".to_vec()]
        );
        assert!(!framer.has_pending_input());
    }

    #[test]
    fn slow_paste_that_keeps_arriving_is_not_cut() {
        let mut framer = RawInputByteFramer::for_host_input();
        let start = std::time::Instant::now();
        let step = PASTE_STALL_TIMEOUT / 2;

        assert!(framer.push_at(b"\x1b[200~one ", start).is_empty());
        assert!(framer.push_at(b"two ", start + step).is_empty());
        assert!(framer.push_at(b"three ", start + step * 2).is_empty());
        let chunks = framer.push_at(b"four\x1b[201~", start + step * 3);

        assert_eq!(
            chunks,
            vec![b"\x1b[200~one two three four\x1b[201~".to_vec()]
        );
    }

    #[test]
    fn paste_variants_end_at_the_byte_or_stall_bound() {
        let start = std::time::Instant::now();
        let mut framer = RawInputByteFramer::for_host_input();
        assert!(framer.push_at(BRACKETED_PASTE_START, start).is_empty());
        assert!(
            framer
                .push_at(&vec![b'a'; MAX_PENDING_PASTE_BYTES], start)
                .is_empty()
        );
        assert!(matches!(framer.held, Held::Paste { .. }));
        assert_eq!(framer.push_at(b"b", start).len(), 1);
        assert!(matches!(framer.held, Held::PasteTail { .. }));
        assert_eq!(
            framer.push_at(b"x", start + PASTE_STALL_TIMEOUT),
            vec![b"x".to_vec()]
        );
        assert!(matches!(framer.held, Held::None));

        let mut stalled = RawInputByteFramer::for_host_input();
        assert!(stalled.push_at(b"\x1b[200~body", start).is_empty());
        assert!(matches!(stalled.held, Held::Paste { .. }));
        assert!(
            stalled
                .push_at(b"", start + PASTE_STALL_TIMEOUT / 2)
                .is_empty()
        );
        assert!(stalled.flush_timeout().is_empty());
        assert_eq!(
            stalled.push_at(b"x", start + PASTE_STALL_TIMEOUT),
            vec![b"\x1b[200~body\x1b[201~".to_vec(), b"x".to_vec()]
        );
        assert!(matches!(stalled.held, Held::None));
    }

    #[test]
    fn oversized_unterminated_paste_is_cut_and_its_tail_dropped() {
        let mut framer = RawInputByteFramer::for_host_input();
        let block = vec![b'a'; 1024 * 1024];

        assert!(framer.push(BRACKETED_PASTE_START).is_empty());
        let mut chunks = Vec::new();
        while chunks.is_empty() {
            chunks = framer.push(&block);
        }

        assert_eq!(chunks.len(), 1);
        let paste = &chunks[0];
        assert!(paste.starts_with(BRACKETED_PASTE_START));
        assert!(paste.ends_with(BRACKETED_PASTE_END));
        assert!(paste.len() > MAX_PENDING_PASTE_BYTES);

        // The rest of the paste, terminator split across reads, is dropped;
        // the key typed after it is not.
        assert!(framer.push(b"tail\x1b[20").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.push(b"1~x"), vec![b"x".to_vec()]);
        assert!(!framer.has_pending_input());
    }

    #[test]
    fn cut_paste_whose_terminator_never_comes_releases_input_after_a_stall() {
        let mut framer = RawInputByteFramer::for_host_input();
        let start = std::time::Instant::now();
        framer.held = Held::PasteTail {
            last_progress: start,
        };

        assert!(framer.push_at(b"more paste", start).is_empty());
        assert_eq!(
            framer.push_at(b"y", start + PASTE_STALL_TIMEOUT),
            vec![b"y".to_vec()]
        );
    }

    #[test]
    fn continuing_cut_paste_tail_outlives_byte_and_idle_flush_counts() {
        let mut framer = RawInputByteFramer::for_host_input();
        let start = std::time::Instant::now();
        framer.held = Held::PasteTail {
            last_progress: start,
        };
        let block = vec![b'a'; MAX_PENDING_PASTE_BYTES];

        // More discarded bytes than the held-paste limit do not make the
        // remainder safe to deliver as keys. Progress prevents stall expiry.
        for _ in 0..4 {
            assert!(framer.push_at(&block, start).is_empty());
            for _ in 0..256 {
                assert!(framer.flush_timeout().is_empty());
            }
            assert!(matches!(framer.held, Held::PasteTail { .. }));
            assert!(framer.buffer.is_empty());
        }

        assert!(framer.push_at(b"tail\x1b[20", start).is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.push_at(b"1~x", start), vec![b"x".to_vec()]);
        assert!(!matches!(framer.held, Held::PasteTail { .. }));
        assert!(!framer.has_pending_input());
    }

    #[test]
    fn partial_suffix_len_finds_split_terminators() {
        assert_eq!(partial_suffix_len(b"abc", BRACKETED_PASTE_END), 0);
        assert_eq!(partial_suffix_len(b"abc\x1b", BRACKETED_PASTE_END), 1);
        assert_eq!(partial_suffix_len(b"abc\x1b[20", BRACKETED_PASTE_END), 4);
        assert_eq!(partial_suffix_len(b"abc\x1b[201", BRACKETED_PASTE_END), 5);
        // A whole terminator is not a partial one.
        assert_eq!(partial_suffix_len(b"abc\x1b[201~", BRACKETED_PASTE_END), 0);
    }

    #[test]
    fn complete_utf8_char_before_incomplete_char_is_drained() {
        let mut framer = RawInputByteFramer::default();
        let mut input = "你".as_bytes().to_vec();
        input.push("好".as_bytes()[0]);

        assert_eq!(framer.push(&input), vec!["你".as_bytes().to_vec()]);
        assert_eq!(framer.push(&[]), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn incomplete_utf8_prefix_is_not_flushed_on_timeout() {
        let mut framer = RawInputByteFramer::default();
        let prefix = &"好".as_bytes()[..1];

        assert!(framer.push(prefix).is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(&"好".as_bytes()[1..]),
            vec!["好".as_bytes().to_vec()]
        );
    }

    #[test]
    fn incomplete_utf8_survives_repeated_idle_flushes_with_or_without_escape() {
        for escaped in [false, true] {
            let mut framer = RawInputByteFramer::for_host_input();
            let mut complete = Vec::new();
            if escaped {
                complete.push(ESC);
            }
            complete.extend_from_slice("好".as_bytes());
            let split = complete.len() - 1;

            assert!(framer.push(&complete[..split]).is_empty());
            for _ in 0..256 {
                assert!(framer.flush_timeout().is_empty());
                assert!(framer.has_pending_input());
            }
            assert_eq!(framer.push(&complete[split..]), vec![complete]);
            assert!(!framer.has_pending_input());
        }
    }

    #[test]
    fn invalid_utf8_lead_byte_does_not_block_trailing_raw_input() {
        let mut framer = RawInputByteFramer::default();

        assert_eq!(
            framer.push(&[0xC0, b'a', b'b']),
            vec![vec![0xC0], vec![b'a'], vec![b'b']]
        );
        assert!(!framer.has_pending_input());
    }

    #[test]
    fn timeout_discards_only_the_incomplete_tail_after_draining_events() {
        let mut framer = RawInputByteFramer::default();

        assert_eq!(
            // A bare `ESC [` would flush as Alt+[; `ESC [ 1 ; 5` is not a key.
            framer.push(b"\xffab\x1b[1;5"),
            vec![vec![0xff], b"a".to_vec(), b"b".to_vec()]
        );
        assert!(framer.flush_timeout().is_empty());
        assert!(!framer.has_pending_input());
    }

    #[test]
    fn complete_utf8_char_before_incomplete_char_survives_timeout_and_next_chunk() {
        let mut framer = RawInputByteFramer::default();
        let mut input = "你".as_bytes().to_vec();
        input.push("好".as_bytes()[0]);

        assert_eq!(framer.push(&input), vec!["你".as_bytes().to_vec()]);
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(&"好".as_bytes()[1..]),
            vec!["好".as_bytes().to_vec()]
        );
    }

    #[test]
    fn alt_utf8_char_drains_as_one_event_before_following_input() {
        let events = parse_raw_input_bytes_sync("\x1béx".as_bytes());
        assert_eq!(events.len(), 2);
        assert_raw_key(
            events.into_iter().next().expect("test precondition"),
            KeyCode::Char('é'),
            KeyModifiers::ALT,
        );
    }

    #[test]
    fn chunked_alt_utf8_waits_for_continuation_byte_after_escape() {
        let mut framer = RawInputFramer::default();
        let bytes = "\x1bé".as_bytes();

        assert!(framer.push(&bytes[..2]).is_empty());
        assert!(framer.flush_timeout().is_empty());
        let events = framer.push(&bytes[2..]);
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().expect("test precondition"),
            KeyCode::Char('é'),
            KeyModifiers::ALT,
        );
    }

    #[test]
    fn chunked_utf8_waits_for_continuation_byte() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(&"é".as_bytes()[..1]).is_empty());
        let events = framer.push(&"é".as_bytes()[1..]);
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().expect("test precondition"),
            KeyCode::Char('é'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn chunked_cjk_utf8_waits_for_all_continuation_bytes() {
        let mut framer = RawInputFramer::default();
        let bytes = "好".as_bytes();

        assert!(framer.push(&bytes[..1]).is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(&bytes[1..2]).is_empty());
        assert!(framer.flush_timeout().is_empty());
        let events = framer.push(&bytes[2..]);
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().expect("test precondition"),
            KeyCode::Char('好'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn chunked_four_byte_utf8_waits_for_all_continuation_bytes() {
        let mut framer = RawInputFramer::default();
        let bytes = "\u{1F642}".as_bytes();

        for split in 1..bytes.len() {
            assert!(framer.push(&bytes[split - 1..split]).is_empty());
            assert!(framer.flush_timeout().is_empty());
        }

        let events = framer.push(&bytes[bytes.len() - 1..]);
        assert_eq!(events.len(), 1);
        assert_raw_key(
            events.into_iter().next().expect("test precondition"),
            KeyCode::Char('\u{1F642}'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn long_multilingual_voice_like_burst_drains_without_truncation() {
        let text =
            "你好，今天我们测试一段比较长的语音输入。こんにちは。안녕하세요.\u{1F642}".repeat(128);
        assert!(
            text.len() > 4096,
            "test input should exceed the client read buffer"
        );
        let mut framer = RawInputByteFramer::default();

        let chunks = framer.push(text.as_bytes());
        let rebuilt: Vec<u8> = chunks.into_iter().flatten().collect();

        assert!(!framer.has_pending_input());
        assert_eq!(rebuilt, text.as_bytes());
    }

    #[test]
    fn long_multilingual_burst_survives_one_byte_chunks_and_timeouts() {
        let text = "中文かなカナ한글\u{1F642}，。".repeat(64);
        let mut framer = RawInputByteFramer::default();
        let mut rebuilt = Vec::new();

        for byte in text.as_bytes() {
            rebuilt.extend(
                framer
                    .push(std::slice::from_ref(byte))
                    .into_iter()
                    .flatten(),
            );
            if framer.has_pending_input() {
                assert!(framer.flush_timeout().is_empty());
            }
        }

        rebuilt.extend(framer.flush_timeout().into_iter().flatten());
        assert!(!framer.has_pending_input());
        assert_eq!(rebuilt, text.as_bytes());
    }

    #[test]
    fn parses_ghostty_default_background_response() {
        let events = parse_raw_input_bytes_sync(b"\x1b]11;rgb:2828/2a2a/3636\x07");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Background,
                color: RgbColor {
                    r: 0x28,
                    g: 0x2a,
                    b: 0x36
                }
            }
        ));
    }

    #[test]
    fn raw_input_framer_reassembles_split_default_background_response() {
        let mut framer = RawInputFramer::default();

        assert!(framer.push(b"\x1b]").is_empty());
        let events = framer.push(b"11;#123456\x07");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Background,
                color: RgbColor {
                    r: 0x12,
                    g: 0x34,
                    b: 0x56,
                }
            }
        ));
    }

    #[test]
    fn incomplete_default_color_replies_enter_bounded_discard_on_idle() {
        for prefix in [b"\x1b]10;".as_slice(), b"\x1b]11;".as_slice()] {
            let mut framer = RawInputByteFramer::default();
            framer.host_color_query_sent();
            assert!(framer.push(prefix).is_empty());
            assert!(matches!(framer.held, Held::ControlString { .. }));
            assert!(framer.flush_timeout().is_empty());
            assert!(matches!(framer.held, Held::ControlTail { .. }));
            assert!(framer.push(b"rgb:1111/2222/3333\x1b").is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert_eq!(framer.push(b"\\x"), vec![b"x".to_vec()]);
            assert!(matches!(framer.held, Held::None));
        }
    }

    #[test]
    fn incomplete_control_strings_reach_the_byte_bound_without_idle() {
        for prefix in [
            b"\x1b]10;".as_slice(),
            b"\x1b]11;".as_slice(),
            b"\x1b]".as_slice(),
            b"\x1bP".as_slice(),
            b"\x1b_".as_slice(),
            b"\x1b^".as_slice(),
            b"\x1bX".as_slice(),
        ] {
            let mut framer = RawInputByteFramer::default();
            assert!(framer.push(prefix).is_empty());
            for _ in prefix.len()..MAX_DISCARDED_CONTROL_TAIL_BYTES - 1 {
                assert!(framer.push(b"1").is_empty());
                assert!(matches!(framer.held, Held::ControlString { .. }));
                assert!(framer.buffer.len() < MAX_DISCARDED_CONTROL_TAIL_BYTES);
            }
            assert!(framer.push(b"1").is_empty());
            assert!(matches!(framer.held, Held::ControlTail { bytes: 0, .. }));
            assert!(framer.buffer.is_empty());
            for _ in 0..MAX_DISCARDED_CONTROL_TAIL_BYTES - 1 {
                assert!(framer.push(b"2").is_empty());
                assert!(matches!(framer.held, Held::ControlTail { .. }));
                assert!(framer.buffer.len() < MAX_DISCARDED_CONTROL_TAIL_BYTES);
            }
            assert!(framer.push(b"2").is_empty());
            assert!(matches!(framer.held, Held::None));
            assert_eq!(framer.push(b"x"), vec![b"x".to_vec()]);
        }
    }

    #[test]
    fn one_read_can_exhaust_both_control_budgets_and_preserve_overflow() {
        for prefix in [b"\x1b]11;".as_slice(), b"\x1bP".as_slice()] {
            let mut framer = RawInputByteFramer::default();
            assert!(framer.push(prefix).is_empty());
            let initial_remaining = MAX_DISCARDED_CONTROL_TAIL_BYTES - prefix.len();
            let burst = vec![b'1'; initial_remaining + MAX_DISCARDED_CONTROL_TAIL_BYTES + 3];
            assert_eq!(framer.push(&burst).concat(), b"111");
            assert!(matches!(framer.held, Held::None));
            assert!(!framer.has_pending_input());
        }
    }

    #[test]
    fn control_tail_budget_is_charged_on_push_and_not_again_on_idle() {
        for prefix in [b"\x1b]".as_slice(), b"\x1bP".as_slice()] {
            let mut framer = RawInputByteFramer::default();
            assert!(framer.push(prefix).is_empty());
            assert!(framer.flush_timeout().is_empty());
            let first = vec![b'1'; MAX_DISCARDED_CONTROL_TAIL_BYTES - 1];
            assert!(framer.push(&first).is_empty());
            assert!(matches!(framer.held, Held::ControlTail { bytes, .. }
                if bytes == first.len()));
            assert_eq!(framer.push(b"2x"), vec![b"x".to_vec()]);
            assert!(matches!(framer.held, Held::None));
        }

        let mut framer = RawInputByteFramer::default();
        assert!(framer.push(b"\x1b]").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"123\x1b").is_empty());
        for _ in 0..4 {
            assert!(framer.flush_timeout().is_empty());
            assert!(matches!(framer.held, Held::ControlTail { bytes: 4, .. }));
        }
        assert_eq!(framer.push(b"\\x"), vec![b"x".to_vec()]);
    }

    #[test]
    fn control_tail_preserves_split_st_at_the_initial_byte_bound() {
        let mut framer = RawInputByteFramer::default();
        let mut prefix = b"\x1bP".to_vec();
        prefix.resize(MAX_DISCARDED_CONTROL_TAIL_BYTES - 1, b'a');
        prefix.push(ESC);
        assert!(framer.push(&prefix).is_empty());
        assert!(matches!(framer.held, Held::ControlTail { .. }));
        assert_eq!(framer.buffer, vec![ESC]);
        assert_eq!(framer.push(b"\\x"), vec![b"x".to_vec()]);
        assert!(matches!(framer.held, Held::None));
    }

    #[test]
    fn implausible_control_tail_ends_at_idle() {
        for prefix in [b"\x1b]".as_slice(), b"\x1bP".as_slice()] {
            let mut framer = RawInputByteFramer::default();
            assert!(framer.push(prefix).is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert!(framer.push(b"a").is_empty());
            assert!(framer.flush_timeout().is_empty());
            assert!(matches!(framer.held, Held::None));
            assert_eq!(framer.push(b"x"), vec![b"x".to_vec()]);
        }
    }

    #[test]
    fn key_prefix_holds_end_at_their_idle_flush_counts() {
        let mut ordinary = RawInputByteFramer::for_host_input();
        assert!(ordinary.push(b"\x1b[").is_empty());
        assert!(matches!(ordinary.held, Held::Sequence));
        assert_eq!(ordinary.flush_timeout(), vec![b"\x1b[".to_vec()]);
        assert!(matches!(ordinary.held, Held::None));

        let mut doubled = RawInputByteFramer::for_host_input();
        assert!(doubled.push(b"\x1b\x1b").is_empty());
        assert!(matches!(doubled.held, Held::DoubledEscape));
        assert_eq!(doubled.flush_timeout().concat(), b"\x1b\x1b");
        assert!(!doubled.has_pending_input());
        assert!(matches!(doubled.held, Held::EscapeReleased));
        assert_eq!(doubled.push(b"x"), vec![b"x".to_vec()]);
        assert!(matches!(doubled.held, Held::None));

        let mut host = RawInputByteFramer::for_host_input();
        host.host_color_query_sent();
        assert!(host.push(b"\x1b").is_empty());
        assert!(host.flush_timeout().is_empty());
        assert!(matches!(host.held, Held::HostReplyPrefix));
        host.host_color_query_sent();
        host.host_cell_size_query_sent();
        assert_eq!(host.flush_timeout(), vec![b"\x1b".to_vec()]);
        assert!(!host.has_pending_input());
    }

    #[test]
    fn incomplete_csi_prefix_is_bounded_across_continuous_input() {
        let mut framer = RawInputByteFramer::default();
        assert!(framer.push(b"\x1b[").is_empty());

        let before_limit = MAX_INCOMPLETE_CSI_BYTES - 1;
        for _ in 2..before_limit {
            assert!(framer.push(b"1").is_empty());
        }
        assert_eq!(framer.buffer.len(), before_limit);
        assert!(matches!(framer.held, Held::Sequence));

        assert!(framer.push(b"1").is_empty());
        assert!(framer.buffer.is_empty());
        assert!(matches!(
            framer.held,
            Held::ControlTail {
                family: ControlStringFamily::HostReplyCsi,
                bytes: 0,
                ..
            }
        ));

        let tail = [b'1'; MAX_DISCARDED_CONTROL_TAIL_BYTES];
        assert!(framer.push(&tail).is_empty());
        assert!(matches!(framer.held, Held::None));
        assert_eq!(framer.push(b"x"), vec![b"x".to_vec()]);

        let mut doubled = RawInputByteFramer::for_host_input();
        assert!(doubled.push(b"\x1b\x1b[").is_empty());
        for _ in 3..before_limit {
            assert!(doubled.push(b"1").is_empty());
        }
        assert_eq!(doubled.buffer.len(), before_limit);
        assert!(matches!(doubled.held, Held::DoubledEscape));
        assert!(doubled.push(b"1").is_empty());
        assert!(doubled.buffer.is_empty());
        assert!(matches!(
            doubled.held,
            Held::ControlTail {
                family: ControlStringFamily::HostReplyCsi,
                bytes: 0,
                ..
            }
        ));
        assert!(doubled.push(&tail).is_empty());
        assert_eq!(doubled.push(b"x"), vec![b"x".to_vec()]);
    }

    #[test]
    fn complete_csi_ss3_and_maximum_coordinate_mouse_survive_split_reads() {
        for sequence in [
            b"\x1b[A".as_slice(),
            b"\x1bOP".as_slice(),
            b"\x1b[<0;65535;65535M".as_slice(),
        ] {
            let mut framer = RawInputFramer::default();
            let mut events = Vec::new();
            for byte in sequence {
                events.extend(framer.push(std::slice::from_ref(byte)));
            }

            assert_eq!(events.len(), 1, "sequence: {sequence:?}");
            match &events[0] {
                RawInputEvent::Key(key) if sequence == b"\x1b[A".as_slice() => {
                    assert_eq!(key.code, KeyCode::Up);
                }
                RawInputEvent::Key(key) if sequence == b"\x1bOP".as_slice() => {
                    assert_eq!(key.code, KeyCode::F(1));
                }
                RawInputEvent::Mouse(mouse) if sequence == b"\x1b[<0;65535;65535M".as_slice() => {
                    assert_eq!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
                    assert_eq!((mouse.column, mouse.row), (u16::MAX - 1, u16::MAX - 1));
                }
                _ => panic!("unexpected event for sequence: {sequence:?}"),
            }
        }
    }

    #[test]
    fn mouse_prefix_holds_end_at_their_idle_flush_counts() {
        let mut sgr = RawInputByteFramer::for_host_input();
        assert!(sgr.push(b"\x1b[<3").is_empty());
        assert!(matches!(sgr.held, Held::MousePrefix));
        assert!(sgr.flush_timeout().is_empty());
        assert!(matches!(sgr.held, Held::MouseTail { .. }));
        assert!(!sgr.has_pending_input());

        let mut delayed = RawInputByteFramer::for_host_input();
        delayed.set_host_escape_disambiguation_active(true);
        assert!(delayed.push(b"\x1b[").is_empty());
        assert!(delayed.flush_timeout().is_empty());
        assert!(matches!(delayed.held, Held::MouseWait { .. }));
        assert_eq!(
            delayed.held_input_flush_timeout_ms(),
            DISAMBIGUATED_MOUSE_TAIL_FLUSH_TIMEOUT_MS
        );
        assert_eq!(delayed.flush_timeout(), vec![b"\x1b[".to_vec()]);
        assert!(matches!(delayed.held, Held::None));
    }

    #[test]
    fn raw_input_byte_framer_discards_split_control_string_after_timeout() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b]").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"11;#123456\x07").is_empty());
        assert_eq!(framer.push(b"a"), vec![b"a".to_vec()]);
    }

    #[test]
    fn raw_input_byte_framer_keeps_discarding_tail_across_timeout() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b]").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"1").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"1;#123456\x07").is_empty());
        assert_eq!(framer.push(b"a"), vec![b"a".to_vec()]);
    }

    #[test]
    fn raw_input_byte_framer_releases_discard_on_implausible_tail() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b]").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"a").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.push(b"b"), vec![b"b".to_vec()]);
    }

    #[test]
    fn parse_raw_input_bytes_sync_does_not_parse_incomplete_strings_as_alt_keys() {
        for bytes in [
            b"\x1b]".as_slice(),
            b"\x1bP".as_slice(),
            b"\x1b_".as_slice(),
            b"\x1b^".as_slice(),
            b"\x1bX".as_slice(),
        ] {
            let events = parse_raw_input_bytes_sync(bytes);

            assert!(events.is_empty(), "parsed {bytes:?} as {events:?}");
        }
    }

    #[test]
    fn non_osc_control_strings_ignore_bel_and_complete_at_st() {
        let bytes = b"\x1bPabc\x07def\x1b\\x";

        let (event, consumed) = extract_one_event(bytes).expect("test precondition");

        assert!(matches!(event, RawInputEvent::Unsupported));
        assert_eq!(consumed, b"\x1bPabc\x07def\x1b\\".len());
    }

    #[test]
    fn non_osc_default_color_text_remains_key_input() {
        let events = parse_raw_input_bytes_sync(b"11;rgb:2828/2a2a/3636\x07");

        assert_eq!(events.len(), 22);
        assert_raw_key(
            events.into_iter().next().expect("test precondition"),
            KeyCode::Char('1'),
            KeyModifiers::empty(),
        );
    }

    #[test]
    fn byte_framer_does_not_hold_non_osc_default_color_text() {
        let mut framer = RawInputByteFramer::default();

        let chunks = framer.push(b"11;rgb:2828");

        assert_eq!(chunks.len(), 11);
        assert!(framer.flush_timeout().is_empty());
    }

    #[test]
    fn holds_lone_escape_and_stitches_split_host_color_reply() {
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();

        // The reply is split right at its ESC introducer.
        assert!(framer.push(b"\x1b").is_empty());
        // The idle flush must not release the ESC as an Escape key while a host
        // color reply is still outstanding.
        assert!(framer.flush_timeout().is_empty());

        // The rest of the OSC 11 reply arrives and stitches back together
        // instead of leaking its payload into the focused pane.
        let chunks = framer.push(b"]11;rgb:2424/2727/3a3a\x1b\\");
        assert_eq!(chunks.len(), 1);
        let (event, _) = extract_one_event(&chunks[0]).expect("test precondition");
        assert!(matches!(
            event,
            RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Background,
                ..
            }
        ));
    }

    #[test]
    fn holds_lone_escape_and_stitches_split_host_cell_size_reply() {
        let mut framer = RawInputByteFramer::default();
        framer.host_cell_size_query_sent();

        // The XTWINOPS reply is split right at its ESC introducer.
        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());

        let chunks = framer.push(b"[6;21;10t");
        assert_eq!(chunks, vec![b"\x1b[6;21;10t".to_vec()]);
        let (event, _) = extract_one_event(&chunks[0]).expect("test precondition");
        assert!(matches!(
            event,
            RawInputEvent::HostCellSizeReport {
                width_px: 10,
                height_px: 21,
            }
        ));
    }

    #[test]
    fn timed_out_host_cell_size_reply_fragments_do_not_leak() {
        for (prefix, tail) in [
            (b"\x1b[6".as_slice(), b";21;10t".as_slice()),
            (b"\x1b[6;".as_slice(), b"21;10t".as_slice()),
            (b"\x1b[6;21;".as_slice(), b"10t".as_slice()),
        ] {
            let mut framer = RawInputByteFramer::default();
            framer.host_cell_size_query_sent();

            assert!(framer.push(prefix).is_empty(), "prefix: {prefix:?}");
            assert!(framer.flush_timeout().is_empty(), "prefix: {prefix:?}");
            assert!(framer.push(tail).is_empty(), "tail: {tail:?}");
            assert_eq!(framer.push(b"a"), vec![b"a".to_vec()]);
        }
    }

    #[test]
    fn split_host_cell_size_reply_after_csi_intro_gets_one_more_flush() {
        let mut framer = RawInputByteFramer::default();
        framer.host_cell_size_query_sent();

        assert!(framer.push(b"\x1b[").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.push(b"6;21;10t"), vec![b"\x1b[6;21;10t".to_vec()]);

        let mut alt_bracket = RawInputByteFramer::default();
        alt_bracket.host_cell_size_query_sent();
        assert!(alt_bracket.push(b"\x1b[").is_empty());
        assert!(alt_bracket.flush_timeout().is_empty());
        assert_eq!(alt_bracket.flush_timeout(), vec![b"\x1b[".to_vec()]);
    }

    #[test]
    fn malformed_host_reply_tail_preserves_following_input() {
        let mut framer = RawInputByteFramer::default();
        framer.host_cell_size_query_sent();

        assert!(framer.push(b"\x1b[6;21").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(b";10xabc"),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
    }

    #[test]
    fn host_reply_tail_discard_is_bounded_across_pushes() {
        let mut framer = RawInputByteFramer::default();
        framer.host_cell_size_query_sent();

        assert!(framer.push(b"\x1b[6;21").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(&[b'1'; 64]).is_empty());
        assert!(matches!(framer.held, Held::ControlTail { bytes: 64, .. }));
        assert_eq!(
            framer.push(&[b'2'; 67]),
            vec![b"2".to_vec(), b"2".to_vec(), b"2".to_vec()]
        );
        assert!(matches!(framer.held, Held::None));
    }

    #[test]
    fn stops_holding_lone_escape_after_host_cell_size_reply_completes() {
        let mut framer = RawInputByteFramer::default();
        framer.host_cell_size_query_sent();

        assert_eq!(
            framer.push(b"\x1b[6;21;10t"),
            vec![b"\x1b[6;21;10t".to_vec()]
        );

        // Window closed: a later lone Escape flushes immediately.
        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn default_byte_framer_does_not_rearm_after_color_scheme_report() {
        let mut framer = RawInputByteFramer::default();

        assert_eq!(
            framer.push(GHOSTTY_COLOR_SCHEME_DARK_REPORT),
            vec![GHOSTTY_COLOR_SCHEME_DARK_REPORT.to_vec()]
        );
        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn opt_in_does_not_delay_plain_escape_without_color_scheme_report() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_color_scheme_change_tracking();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn opted_in_byte_framer_rearms_after_outer_focus_gained() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_color_scheme_change_tracking();
        framer.enable_host_appearance_query_on_focus();

        assert_eq!(framer.push(b"\x1b[I"), vec![b"\x1b[I".to_vec()]);
        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(b"[?997;2n"),
            vec![GHOSTTY_COLOR_SCHEME_LIGHT_REPORT.to_vec()]
        );
    }

    #[test]
    fn opted_in_byte_framer_reassembles_appearance_reply_split_after_csi() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_color_scheme_change_tracking();
        framer.enable_host_appearance_query_on_focus();

        assert_eq!(framer.push(b"\x1b[I"), vec![b"\x1b[I".to_vec()]);
        assert!(framer.push(b"\x1b[").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(b"?997;2n"),
            vec![GHOSTTY_COLOR_SCHEME_LIGHT_REPORT.to_vec()]
        );
    }

    #[test]
    fn opted_in_byte_framer_reassembles_delayed_appearance_reply() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_color_scheme_change_tracking();
        framer.enable_host_appearance_query_on_focus();

        assert_eq!(framer.push(b"\x1b[I"), vec![b"\x1b[I".to_vec()]);
        assert!(framer.push(b"\x1b[?997;").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(b"2n"),
            vec![GHOSTTY_COLOR_SCHEME_LIGHT_REPORT.to_vec()]
        );
    }

    #[test]
    fn timed_out_appearance_reply_preserves_pending_color_reply_window() {
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();
        framer.enable_host_appearance_query_on_focus();

        assert_eq!(framer.push(b"\x1b[I"), vec![b"\x1b[I".to_vec()]);
        assert!(framer.push(b"\x1b[?997;").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert!(framer.push(b"2n").is_empty());

        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(
            framer.push(b"]10;rgb:aaaa/bbbb/cccc\x1b\\"),
            vec![b"\x1b]10;rgb:aaaa/bbbb/cccc\x1b\\".to_vec()]
        );
    }

    #[test]
    fn disabled_focus_query_does_not_rearm_byte_framer() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_color_scheme_change_tracking();

        assert_eq!(framer.push(b"\x1b[I"), vec![b"\x1b[I".to_vec()]);
        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn focus_query_policy_does_not_delay_plain_escape_without_focus() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_appearance_query_on_focus();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn focus_query_without_reply_holds_escape_for_only_one_flush() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_appearance_query_on_focus();

        assert_eq!(framer.push(b"\x1b[I"), vec![b"\x1b[I".to_vec()]);
        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn opted_in_byte_framer_rearms_after_color_scheme_report() {
        let mut framer = RawInputByteFramer::default();
        framer.enable_host_color_scheme_change_tracking();

        assert_eq!(
            framer.push(GHOSTTY_COLOR_SCHEME_DARK_REPORT),
            vec![GHOSTTY_COLOR_SCHEME_DARK_REPORT.to_vec()]
        );

        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        let chunks = framer.push(b"]10;#abcdef\x07");
        assert_eq!(chunks.len(), 1);
        let (event, _) = extract_one_event(&chunks[0]).expect("test precondition");
        assert!(matches!(
            event,
            RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Foreground,
                color: RgbColor {
                    r: 0xab,
                    g: 0xcd,
                    b: 0xef
                }
            }
        ));

        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        let chunks = framer.push(b"]11;#123456\x07");
        assert_eq!(chunks.len(), 1);
        let (event, _) = extract_one_event(&chunks[0]).expect("test precondition");
        assert!(matches!(
            event,
            RawInputEvent::HostDefaultColor {
                kind: DefaultColorKind::Background,
                color: RgbColor {
                    r: 0x12,
                    g: 0x34,
                    b: 0x56
                }
            }
        ));

        assert!(framer.push(b"\x1b").is_empty());
        assert!(framer.flush_timeout().is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn flushes_lone_escape_when_not_awaiting_host_color_reply() {
        let mut framer = RawInputByteFramer::default();

        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn gives_up_holding_lone_escape_after_one_idle_flush() {
        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();

        assert!(framer.push(b"\x1b").is_empty());
        // First idle flush holds the escape.
        assert!(framer.flush_timeout().is_empty());
        // No continuation arrived; the second idle flush releases it as Escape.
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }

    #[test]
    fn stops_holding_lone_escape_after_host_color_reply_completes() {
        use std::fmt::Write as _;

        let mut framer = RawInputByteFramer::default();
        framer.host_color_query_sent();
        let mut replies =
            String::from("\x1b]10;rgb:6565/7b7b/8383\x1b\\\x1b]11;rgb:2424/2727/3a3a\x1b\\");
        for index in 0..=u8::MAX {
            write!(replies, "\x1b]4;{index};rgb:1111/2222/3333\x1b\\")
                .expect("writing into a String cannot fail");
        }

        let chunks = framer.push(replies.as_bytes());
        assert_eq!(chunks.len(), 258);

        // Window closed: a later lone Escape flushes immediately.
        assert!(framer.push(b"\x1b").is_empty());
        assert_eq!(framer.flush_timeout(), vec![b"\x1b".to_vec()]);
    }
}
