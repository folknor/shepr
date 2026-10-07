//! Byte-level scanner for the few control sequences vte never hands to a
//! `Handler`, but shepr must answer or track: OSC 7 / OSC 9;9 / OSC 1337
//! CurrentDir working-directory reports, OSC 9;4 progress (agent detection
//! evidence), the bodies of complete OSCs while capture is on (the pane's
//! opt-in OSC debug log, which therefore sees the framing the terminal
//! used), CSI ? 996 n, CSI 16 t, XTGETTCAP
//! (`ESC P + q`), `CSI ? 3 J`, and the modifyOtherKeys spellings vte drops
//! (`CSI > m`, `CSI > 4 n`, `CSI > 4 ; Pv m` with Pv above 2).
//! The scanner also recognizes an OSC 52 store when its payload hits the
//! parser bound, so truncated base64 still gets a size diagnostic.
//!
//! Everything vte does dispatch (private modes, DECRQM, RIS, the vte-parsed
//! modifyOtherKeys forms, printed characters) is handled by the parser's
//! `Handler` wrapper in `handler.rs` instead, where it stays in byte order
//! inside synchronized updates too. Do not add sequences here that vte
//! dispatches.
//!
//! The scanner follows vte's framing for sequences that can produce scan
//! events. It collapses uninspected DCS states when their difference cannot
//! affect those events. vte only understands 7-bit controls: in
//! ground state every byte other than ESC is text or a no-op control (raw C1
//! bytes such as 0x90 are executed as no-ops, never open a sequence), so the
//! scanner only leaves ground on ESC. Inside sequences: OSC ends on BEL, ESC,
//! CAN or SUB; inspected DCS passthrough ends on ESC, CAN, SUB or raw 0x9C.
//! For other DCS forms, the scanner merges vte's ignore and passthrough states
//! because their raw-0x9C distinction cannot change our events. SOS/PM/APC
//! strings end on ESC, CAN or SUB. Events carry a consumed-byte boundary
//! relative to the slice handed to [`Scanner::scan`]. Completed reports use
//! the boundary after their terminator. An OSC cut short for the parser marks
//! the boundary before the first byte to skip, and its resume marks the end of
//! the skipped input: past a BEL, CAN or SUB terminator, before an ESC one. So
//! callers can keep parser input and scanner effects in byte order.

use crate::limits::{
    MAX_CLIPBOARD_BYTES, MAX_CSI_BYTES, MAX_DCS_INTRO_BYTES, MAX_OSC_BYTES, MAX_OSC_RAW_BYTES,
    MAX_U16_DECIMAL_DIGITS, MAX_XTGETTCAP_BYTES, XTGETTCAP_REPLY_OVERHEAD_BYTES,
};
use memchr::memchr;

const XTGETTCAP_RGB_BITS_PER_CHANNEL: &[u8] = b"8";
const XTGETTCAP_SETRGBF: &[u8] = b"\\E[38:2:%p1%d:%p2%d:%p3%dm";
const XTGETTCAP_SETRGBB: &[u8] = b"\\E[48:2:%p1%d:%p2%d:%p3%dm";

fn is_base64_byte(byte: u8) -> bool {
    matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/')
}

/// Raw OSC working-directory report. OSC 7 carries a URI; the other supported
/// reports carry paths. Parsing belongs to the pane after the scanner frames it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkingDirectoryReport {
    Uri(Vec<u8>),
    Path(Vec<u8>),
}

/// The state of a ConEmu `OSC 9 ; 4 ; state ; percent` progress report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressState {
    /// 0: the progress indicator is removed.
    Remove,
    /// 1: normal progress with a percentage.
    Normal,
    /// 2: error.
    Error,
    /// 3: indeterminate progress.
    Indeterminate,
    /// 4: paused or warning.
    Paused,
}

impl ProgressState {
    fn from_code(code: u16) -> Option<Self> {
        Some(match code {
            0 => Self::Remove,
            1 => Self::Normal,
            2 => Self::Error,
            3 => Self::Indeterminate,
            4 => Self::Paused,
            _ => return None,
        })
    }

    /// The state's ConEmu code.
    pub const fn code(self) -> u8 {
        match self {
            Self::Remove => 0,
            Self::Normal => 1,
            Self::Error => 2,
            Self::Indeterminate => 3,
            Self::Paused => 4,
        }
    }
}

/// A ConEmu `OSC 9 ; 4` progress report, parsed once by the scanner. A report
/// whose state is missing or not one of the five ConEmu states is not a
/// progress report. A percentage that is absent or not a decimal `u8` is
/// `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub state: ProgressState,
    pub percent: Option<u8>,
}

impl Progress {
    fn parse(payload: &[u8]) -> Option<Self> {
        let mut params = payload.strip_prefix(b"4;")?.split(|byte| *byte == b';');
        let state = ProgressState::from_code(parse_decimal(params.next()?)?)?;
        let percent = params
            .next()
            .and_then(parse_decimal)
            .and_then(|percent| u8::try_from(percent).ok());
        Some(Self { state, percent })
    }
}

/// The canonical `4;state[;percent]` spelling, which is what detection
/// manifests match.
impl std::fmt::Display for Progress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "4;{}", self.state.code())?;
        if let Some(percent) = self.percent {
            write!(f, ";{percent}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ScanEvent {
    /// An OSC exceeded the adapter's payload bound. The parser must be ended
    /// here, and its input skipped until the scanner reaches the real end. A
    /// recognized OSC 52 clipboard store also carries a decoded-size lower
    /// bound so it can be diagnosed even when the cut base64 cannot decode.
    AbortOversizedOsc {
        clipboard_store_bytes_at_least: Option<usize>,
    },
    /// The real terminator of an OSC that the parser was forced to end early.
    ResumeAfterOversizedOsc,
    /// CSI ? 996 n (color scheme DSR).
    ColorSchemeQuery,
    /// CSI 16 t (cell size in pixels).
    CellSizeQuery,
    /// Complete XTGETTCAP replies, in request order.
    Xtgettcap(Vec<Vec<u8>>),
    /// Working-directory report payload, exactly as sent.
    WorkingDirectory(WorkingDirectoryReport),
    /// ConEmu progress report (`OSC 9 ; 4 ; state ; percent`).
    Progress(Progress),
    /// The body of a complete OSC, exactly as the parser framed it. Only
    /// emitted while body capture is on (the pane's opt-in OSC debug log).
    OscBody(Vec<u8>),
    /// CSI ? 3 J: the DECSED spelling of ED3 (erase scrollback). vte only
    /// dispatches `CSI 3 J`, but programs (Droid among them) emit this form.
    EraseScrollback,
    /// xterm modifyOtherKeys level (0, 1 or 2) set by a spelling vte does not
    /// dispatch: `CSI > m`, `CSI > 4 n`, or `CSI > 4 ; Pv m` with Pv > 2.
    ModifyOtherKeys(super::ModifyOtherKeysLevel),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ScannedEvent {
    pub(super) end: usize,
    pub(super) event: ScanEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    Osc,
    DcsIntro,
    XtgettcapBody,
    DcsIgnore,
    StringIgnore,
}

#[derive(Debug, Default)]
pub(super) struct Scanner {
    state: State,
    buffer: Vec<u8>,
    overflow: bool,
    /// Bytes stored in vte's current OSC raw payload, excluding `;` separators.
    osc_raw_bytes: usize,
    /// Separators observed in the current OSC. OSC 52's supported store form
    /// has exactly two before its base64 payload.
    osc_separators: usize,
    /// Whether the current OSC has the `52;c;`, `52;p;` or `52;s;` store
    /// prefix, and whether every payload byte so far could be base64.
    osc52_store_payload: bool,
    osc52_payload_is_base64: bool,
    /// The current OSC passed `MAX_OSC_RAW_BYTES` and the parser was ended.
    osc_cut: bool,
    /// Emit every complete OSC body as [`ScanEvent::OscBody`].
    capture_osc_bodies: bool,
}

impl Scanner {
    pub(super) fn set_capture_osc_bodies(&mut self, capture: bool) {
        self.capture_osc_bodies = capture;
    }

    pub(super) fn has_oversized_osc(&self) -> bool {
        self.state == State::Osc && self.osc_cut
    }

    pub(super) fn scan(&mut self, bytes: &[u8]) -> Vec<ScannedEvent> {
        let mut events = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            if self.state == State::Ground {
                // Fast path: only ESC leaves ground state.
                match memchr(0x1b, &bytes[index..]) {
                    Some(offset) => index += offset,
                    None => break,
                }
            }
            let byte = bytes[index];
            self.step(byte, index, &mut events);
            index += 1;
        }
        events
    }

    fn step(&mut self, byte: u8, index: usize, events: &mut Vec<ScannedEvent>) {
        match self.state {
            State::Ground => {
                if byte == 0x1b {
                    self.enter(State::Escape);
                }
            }
            State::Escape => self.escape(byte),
            State::EscapeIntermediate => match byte {
                0x1b => self.enter(State::Escape),
                0x18 | 0x1a | 0x30..=0x7e => self.enter(State::Ground),
                _ => {}
            },
            State::Csi => match byte {
                0x1b => self.enter(State::Escape),
                0x18 | 0x1a => self.enter(State::Ground),
                0x40..=0x7e => {
                    if !self.overflow {
                        self.dispatch_csi(byte, index, events);
                    }
                    self.enter(State::Ground);
                }
                0x20..=0x3f => {
                    if self.buffer.len() >= MAX_CSI_BYTES {
                        self.overflow = true;
                    } else {
                        self.buffer.push(byte);
                    }
                }
                // C0 controls inside CSI are executed by the core and do not
                // change the sequence; DEL and high bytes are ignored.
                _ => {}
            },
            State::Osc => match byte {
                0x07 | 0x18 | 0x1a => {
                    if self.osc_cut {
                        // The parser already left the OSC at the cut; this
                        // terminator is skipped with the body, so a BEL does
                        // not reach the parser as a bell.
                        events.push(ScannedEvent {
                            end: index + 1,
                            event: ScanEvent::ResumeAfterOversizedOsc,
                        });
                    } else {
                        self.dispatch_osc(index, events);
                    }
                    self.enter(State::Ground);
                }
                0x1b => {
                    if self.osc_cut {
                        // The ESC starts what follows, so the parser gets it.
                        events.push(ScannedEvent {
                            end: index,
                            event: ScanEvent::ResumeAfterOversizedOsc,
                        });
                    } else {
                        self.dispatch_osc(index, events);
                    }
                    self.enter(State::Escape);
                }
                0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1c..=0x1f => {}
                _ => {
                    if self.osc_cut {
                        return;
                    }
                    if byte != b';' && self.osc_raw_bytes >= MAX_OSC_RAW_BYTES {
                        events.push(ScannedEvent {
                            // Stop before the first byte past the parser's
                            // bound.
                            end: index,
                            event: ScanEvent::AbortOversizedOsc {
                                clipboard_store_bytes_at_least: self
                                    .cut_clipboard_store_size_lower_bound(byte),
                            },
                        });
                        self.osc_cut = true;
                        return;
                    }
                    if byte == b';' {
                        if self.osc_separators == 1 {
                            let prefix = self.buffer.as_slice();
                            self.osc52_store_payload =
                                prefix == b"52;c" || prefix == b"52;p" || prefix == b"52;s";
                            self.osc52_payload_is_base64 = true;
                        }
                        self.osc_separators = self.osc_separators.saturating_add(1);
                    } else {
                        if self.osc52_store_payload && self.osc_separators == 2 {
                            // Padding is only valid at the end of a complete
                            // base64 payload. A store still has bytes past
                            // this cut, so an earlier `=` cannot decode.
                            if !is_base64_byte(byte) || byte == b'=' {
                                self.osc52_payload_is_base64 = false;
                            }
                        }
                        self.osc_raw_bytes += 1;
                    }
                    if self.buffer.len() >= MAX_OSC_BYTES {
                        self.overflow = true;
                    } else {
                        self.buffer.push(byte);
                    }
                }
            },
            // vte's DCS entry/param/intermediate states: only ESC, CAN and
            // SUB leave them early (a raw 0x9C is ignored here).
            State::DcsIntro => match byte {
                0x1b => self.enter(State::Escape),
                0x18 | 0x1a => self.enter(State::Ground),
                0x40..=0x7e => {
                    let is_xtgettcap = byte == b'q' && self.buffer.as_slice() == b"+";
                    if is_xtgettcap {
                        self.enter(State::XtgettcapBody);
                    } else {
                        self.enter(State::DcsIgnore);
                    }
                }
                0x20..=0x3f => {
                    if self.buffer.len() >= MAX_DCS_INTRO_BYTES {
                        self.overflow = true;
                    } else {
                        self.buffer.push(byte);
                    }
                }
                _ => {}
            },
            State::XtgettcapBody => match byte {
                0x1b => {
                    self.finish_xtgettcap(index, events);
                    self.enter(State::Escape);
                }
                0x9c => {
                    self.finish_xtgettcap(index, events);
                    self.enter(State::Ground);
                }
                0x18 | 0x1a => self.enter(State::Ground),
                // vte's DCS passthrough drops DEL and high bytes other than
                // ST. They must not invalidate an otherwise valid capability.
                0x7f | 0x80..=0x9b | 0x9d..=0xff => {}
                _ => {
                    if self.buffer.len() >= MAX_XTGETTCAP_BYTES {
                        self.overflow = true;
                    } else {
                        self.buffer.push(byte);
                    }
                }
            },
            // We collapse vte's DcsIgnore and DcsPassthrough here. Their raw
            // 0x9C difference cannot change scanner events: both resume on ESC.
            State::DcsIgnore => match byte {
                0x1b => self.enter(State::Escape),
                0x18 | 0x1a | 0x9c => self.enter(State::Ground),
                _ => {}
            },
            State::StringIgnore => match byte {
                0x1b => self.enter(State::Escape),
                0x18 | 0x1a => self.enter(State::Ground),
                _ => {}
            },
        }
    }

    fn escape(&mut self, byte: u8) {
        match byte {
            0x20..=0x2f => self.enter(State::EscapeIntermediate),
            b'[' => self.enter(State::Csi),
            b']' => self.enter(State::Osc),
            b'P' => self.enter(State::DcsIntro),
            b'X' | b'^' | b'_' => self.enter(State::StringIgnore),
            0x18 | 0x1a | 0x30..=0x7e => self.enter(State::Ground),
            // ESC ESC restarts the escape and other C0 controls execute in
            // place; vte ignores DEL and high bytes while an escape is pending.
            _ => {}
        }
    }

    fn enter(&mut self, state: State) {
        self.state = state;
        self.buffer.clear();
        self.overflow = false;
        self.osc_raw_bytes = 0;
        self.osc_separators = 0;
        self.osc52_store_payload = false;
        self.osc52_payload_is_base64 = false;
        self.osc_cut = false;
    }

    fn cut_clipboard_store_size_lower_bound(&self, next_byte: u8) -> Option<usize> {
        if self.osc_separators != 2
            || !self.osc52_store_payload
            || !self.osc52_payload_is_base64
            || !is_base64_byte(next_byte)
        {
            return None;
        }

        // A supported target contributes three non-separator bytes (`52`
        // and `c`, `p` or `s`). Count only complete base64 quartets so the
        // result remains a lower bound even though the original terminator
        // and any remaining payload have not arrived yet.
        let encoded_bytes = self.osc_raw_bytes.checked_sub(3)?;
        let decoded_lower_bound = encoded_bytes / 4 * 3;
        (decoded_lower_bound > MAX_CLIPBOARD_BYTES).then_some(decoded_lower_bound)
    }

    fn dispatch_csi(&mut self, final_byte: u8, index: usize, events: &mut Vec<ScannedEvent>) {
        let params = self.buffer.as_slice();
        let event = match final_byte {
            b'n' if crate::seq::is_color_scheme_query(params, final_byte) => {
                Some(ScanEvent::ColorSchemeQuery)
            }
            b'J' if params == b"?3" => Some(ScanEvent::EraseScrollback),
            // CSI > 4 n: modifyOtherKeys back to its default (off).
            b'n' if params
                .strip_prefix(b">")
                .is_some_and(|resource| parse_decimal(resource) == Some(4)) =>
            {
                Some(ScanEvent::ModifyOtherKeys(super::ModifyOtherKeysLevel::Off))
            }
            b'm' => params
                .strip_prefix(b">")
                .and_then(undispatched_modify_other_keys_level)
                .map(ScanEvent::ModifyOtherKeys),
            b't' => {
                let first = params
                    .split(|byte| *byte == b';')
                    .next()
                    .unwrap_or_default();
                (first.iter().all(u8::is_ascii_digit) && parse_decimal(first) == Some(16))
                    .then_some(ScanEvent::CellSizeQuery)
            }
            _ => None,
        };
        if let Some(event) = event {
            events.push(ScannedEvent {
                end: index + 1,
                event,
            });
        }
    }

    fn dispatch_osc(&mut self, index: usize, events: &mut Vec<ScannedEvent>) {
        if self.overflow {
            return;
        }
        let body = self.buffer.as_slice();
        if self.capture_osc_bodies {
            events.push(ScannedEvent {
                end: index + 1,
                event: ScanEvent::OscBody(body.to_vec()),
            });
        }
        let report = body
            .strip_prefix(b"7;")
            .map(|value| WorkingDirectoryReport::Uri(value.to_vec()))
            .or_else(|| {
                body.strip_prefix(b"9;9;")
                    .map(|value| WorkingDirectoryReport::Path(value.to_vec()))
            })
            .or_else(|| {
                body.strip_prefix(b"1337;CurrentDir=")
                    .map(|value| WorkingDirectoryReport::Path(value.to_vec()))
            });
        if let Some(payload) = report {
            events.push(ScannedEvent {
                end: index + 1,
                event: ScanEvent::WorkingDirectory(payload),
            });
            return;
        }
        // ConEmu progress is `OSC 9 ; 4 ; state ; percent`. Every other OSC 9
        // is an iTerm2-style notification (or ConEmu's other subcommands),
        // which must not overwrite progress evidence.
        if let Some(progress) = body.strip_prefix(b"9;").and_then(Progress::parse) {
            events.push(ScannedEvent {
                end: index + 1,
                event: ScanEvent::Progress(progress),
            });
        }
    }

    fn finish_xtgettcap(&mut self, index: usize, events: &mut Vec<ScannedEvent>) {
        if self.overflow {
            return;
        }
        let replies: Vec<Vec<u8>> = self
            .buffer
            .split(|byte| *byte == b';')
            .filter_map(xtgettcap_response)
            .collect();
        if !replies.is_empty() {
            events.push(ScannedEvent {
                end: index + 1,
                event: ScanEvent::Xtgettcap(replies),
            });
        }
    }
}

/// The modifyOtherKeys level set by `CSI > params m` (XTMODKEYS), for the
/// spellings vte does not dispatch to its `Handler`: a bare `CSI > m` (resets
/// every resource) and `CSI > 4 ; Pv m` with Pv above 2 (clamped to 2). vte
/// dispatches `CSI > 4 ; Pv m` for Pv 0..=2 itself (a missing or empty Pv is
/// 0); reporting those here too would apply them twice and out of order.
fn undispatched_modify_other_keys_level(params: &[u8]) -> Option<super::ModifyOtherKeysLevel> {
    if params.is_empty() {
        return Some(super::ModifyOtherKeysLevel::Off);
    }
    let mut parts = params.split(|byte| *byte == b';');
    let resource = parts.next().unwrap_or_default();
    let value = parts.next();
    if parts.next().is_some() || parse_decimal(resource) != Some(4) {
        return None;
    }
    let level = value.and_then(parse_decimal)?;
    (level > 2).then_some(super::ModifyOtherKeysLevel::All)
}

fn parse_decimal(bytes: &[u8]) -> Option<u16> {
    if bytes.is_empty() || bytes.len() > MAX_U16_DECIMAL_DIGITS {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

fn xtgettcap_response(cap_hex: &[u8]) -> Option<Vec<u8>> {
    if cap_hex.is_empty() || !cap_hex.len().is_multiple_of(2) {
        return None;
    }

    let mut normalized_cap_hex = Vec::with_capacity(cap_hex.len());
    for &byte in cap_hex {
        if !byte.is_ascii_hexdigit() {
            return None;
        }
        normalized_cap_hex.push(byte.to_ascii_uppercase());
    }

    let value = xtgettcap_value(&normalized_cap_hex)?;
    Some(build_xtgettcap_response(&normalized_cap_hex, value))
}

/// Capabilities this pane can stand behind, keyed by upper-case hex name.
/// `Some(None)` is a boolean capability, `Some(Some(v))` a string/number.
/// Unknown names get no reply at all.
fn xtgettcap_value(cap_hex: &[u8]) -> Option<Option<&'static [u8]>> {
    match cap_hex {
        // TN: terminal name.
        b"544E" => Some(Some(super::PANE_TERM.as_bytes())),
        // limits-exempt: XTGETTCAP requires the indexed palette size as decimal bytes.
        // Co / colors: palette size.
        b"436F" | b"636F6C6F7273" => Some(Some(b"256")),
        // Tc: truecolor, matching the pane's COLORTERM value; Su: styled
        // underlines. Both are boolean capabilities.
        b"5463" | b"5375" => Some(None),
        // RGB: bits per channel.
        b"524742" => Some(Some(XTGETTCAP_RGB_BITS_PER_CHANNEL)),
        // setrgbf / setrgbb.
        b"73657472676266" => Some(Some(XTGETTCAP_SETRGBF)),
        b"73657472676262" => Some(Some(XTGETTCAP_SETRGBB)),
        // Ms: OSC 52 clipboard.
        b"4D73" => Some(Some(b"\\E]52;%p1%s;%p2%s\\007")),
        // Smulx: underline style.
        b"536D756C78" => Some(Some(b"\\E[4:%p1%dm")),
        // Setulc: underline color.
        // limits-exempt: this terminfo string encodes fixed RGB channel conversion parameters.
        b"536574756C63" => Some(Some(
            b"\\E[58:2::%p1%{65536}%/%d:%p1%{256}%/%{255}%&%d:%p1%{255}%&%d%;m",
        )),
        _ => None,
    }
}

fn build_xtgettcap_response(cap_hex: &[u8], value: Option<&[u8]>) -> Vec<u8> {
    // Each value byte expands to two wire hex digits.
    let mut response = Vec::with_capacity(
        XTGETTCAP_REPLY_OVERHEAD_BYTES + cap_hex.len() + value.map_or(0, |bytes| bytes.len() * 2),
    );
    response.extend_from_slice(b"\x1bP1+r");
    response.extend_from_slice(cap_hex);
    if let Some(value) = value {
        response.push(b'=');
        append_upper_hex(value, &mut response);
    }
    response.extend_from_slice(b"\x1b\\");
    response
}

fn append_upper_hex(bytes: &[u8], output: &mut Vec<u8>) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &byte in bytes {
        output.push(HEX[usize::from(byte >> 4)]);
        output.push(HEX[usize::from(byte & 0x0f)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_chunks(chunks: &[&[u8]]) -> Vec<ScannedEvent> {
        let mut scanner = Scanner::default();
        let mut events = Vec::new();
        let mut offset = 0;
        for chunk in chunks {
            events.extend(scanner.scan(chunk).into_iter().map(|mut event| {
                event.end += offset;
                event
            }));
            offset += chunk.len();
        }
        events
    }

    fn assert_chunk_equivalence(bytes: &[u8]) {
        let expected = scan_chunks(&[bytes]);
        let one_byte_chunks: Vec<_> = bytes.chunks(1).collect();
        assert_eq!(
            scan_chunks(&one_byte_chunks),
            expected,
            "bytewise: {bytes:?}"
        );
        for split in 0..=bytes.len() {
            assert_eq!(
                scan_chunks(&[&bytes[..split], &bytes[split..]]),
                expected,
                "split {split}: {bytes:?}"
            );
        }
    }

    fn scanned_events(bytes: &[u8]) -> Vec<ScanEvent> {
        scan_chunks(&[bytes])
            .into_iter()
            .map(|event| event.event)
            .collect()
    }

    #[test]
    fn modes_and_reset_are_left_to_the_parser_handler() {
        // vte dispatches all of these to the `Handler`; the scanner must not
        // report them a second time, out of sync-update order.
        let bytes: &[u8] =
            b"\x1b[?9h\x1b[?1016;2031h\x1b[?2048l\x1b[?1016$p\x1bc\x1b[>4;2m\x1b[?4m";
        assert!(scan_chunks(&[bytes]).is_empty());
    }

    #[test]
    fn xtgettcap_replies_for_7bit_intro_with_either_terminator() {
        for bytes in [
            b"\x1bP+q5463;524742\x1b\\".as_slice(),
            b"\x1bP+q5463;524742\x9c".as_slice(),
        ] {
            let events = scan_chunks(&[bytes]);
            assert_eq!(events.len(), 1, "{bytes:?}");
            let ScanEvent::Xtgettcap(replies) = &events[0].event else {
                panic!("expected xtgettcap reply for {bytes:?}");
            };
            assert_eq!(replies.len(), 2);
            assert_eq!(replies[0], b"\x1bP1+r5463\x1b\\");
            assert_chunk_equivalence(bytes);
        }
    }

    #[test]
    fn xtgettcap_truecolor_values_follow_pane_color_identity() {
        assert_eq!(super::super::PANE_COLORTERM, "truecolor");
        assert_eq!(xtgettcap_value(b"5463"), Some(None));
        assert_eq!(xtgettcap_value(b"524742"), Some(Some(b"8".as_slice())));
        assert!(xtgettcap_value(b"73657472676266").is_some());
        assert!(xtgettcap_value(b"73657472676262").is_some());
    }

    #[test]
    fn xtgettcap_ignores_del_and_high_bytes_in_the_body() {
        let bytes: &[u8] = b"\x1bP+q54\x7f\x80\xff63\x1b\\";
        let events = scan_chunks(&[bytes]);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].event,
            ScanEvent::Xtgettcap(vec![b"\x1bP1+r5463\x1b\\".to_vec()])
        );
        assert_chunk_equivalence(bytes);
    }

    /// vte only understands 7-bit controls: a raw C1 DCS (0x90) is executed as
    /// a no-op and its payload printed as text, so the scanner must neither
    /// answer it nor sit in DCS state until the next ESC.
    #[test]
    fn raw_c1_dcs_is_text_like_the_core() {
        let bytes: &[u8] = b"\x90+q5463\x9c\x90+q5463\x1b[?996n";
        assert_eq!(scanned_events(bytes), vec![ScanEvent::ColorSchemeQuery]);
        assert_chunk_equivalence(bytes);
    }

    #[test]
    fn xtgettcap_completes_at_escape_like_the_core() {
        let bytes: &[u8] = b"\x1bP+q4d73\x1b\\";
        let events = scan_chunks(&[bytes]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].end, bytes.len() - 1);
    }

    #[test]
    fn oversized_osc_marks_abort_and_resume_boundaries() {
        let mut scanner = Scanner::default();
        let mut events = scanner.scan(b"\x1b]7;");
        // vte stores `7` in its raw OSC buffer, but not either `;` separator.
        // The adapter retains no report at this size, but the parser gets its
        // entire raw-byte allowance before the next ordinary body byte is cut.
        let mut body = vec![b'x'; MAX_OSC_RAW_BYTES - 1];
        let separator = body.len() / 2;
        body.insert(separator, b';');
        events.extend(scanner.scan(&body));
        assert!(events.is_empty());
        assert!(!scanner.has_oversized_osc());

        events = scanner.scan(b"x");
        assert_eq!(
            events,
            vec![ScannedEvent {
                end: 0,
                event: ScanEvent::AbortOversizedOsc {
                    clipboard_store_bytes_at_least: None,
                },
            }]
        );

        events = scanner.scan(b"more body");
        assert!(events.is_empty());
        events = scanner.scan(b"\x1b\\\x1b[?996n");
        assert_eq!(
            events,
            vec![
                ScannedEvent {
                    end: 0,
                    event: ScanEvent::ResumeAfterOversizedOsc,
                },
                ScannedEvent {
                    end: 9,
                    event: ScanEvent::ColorSchemeQuery,
                },
            ]
        );

        // A BEL terminator is skipped with the body.
        let mut scanner = Scanner::default();
        let mut events = scanner.scan(b"\x1b]");
        events.extend(scanner.scan(&vec![b'x'; MAX_OSC_RAW_BYTES + 1]));
        events.extend(scanner.scan(b"\x07"));
        assert_eq!(
            events,
            vec![
                ScannedEvent {
                    end: MAX_OSC_RAW_BYTES,
                    event: ScanEvent::AbortOversizedOsc {
                        clipboard_store_bytes_at_least: None,
                    },
                },
                ScannedEvent {
                    end: 1,
                    event: ScanEvent::ResumeAfterOversizedOsc,
                },
            ]
        );
    }

    #[test]
    fn an_osc_past_the_retention_bound_is_not_dispatched_but_not_cut() {
        let mut scanner = Scanner::default();
        let mut bytes = b"\x1b]7;".to_vec();
        bytes.resize(bytes.len() + MAX_OSC_BYTES, b'x');
        bytes.push(0x07);
        assert!(scanner.scan(&bytes).is_empty());
    }

    #[test]
    fn unknown_xtgettcap_names_are_silent() {
        assert!(scan_chunks(&[b"\x1bP+q6E6F7065;4D7\x1b\\".as_slice()]).is_empty());
    }

    #[test]
    fn c1_bytes_inside_utf8_are_text() {
        // U+00D0 is C3 90 and U+00DC is C3 9C: neither may open or close a DCS.
        let mut bytes = "\u{d0}+q5463\u{dc}".as_bytes().to_vec();
        bytes.extend_from_slice(b"\x1b[?996n");
        let events = scan_chunks(&[bytes.as_slice()]);
        assert_eq!(
            events,
            vec![ScannedEvent {
                end: bytes.len(),
                event: ScanEvent::ColorSchemeQuery,
            }]
        );
        assert_chunk_equivalence(&bytes);
    }

    #[test]
    fn working_directory_reports_cover_osc7_conemu_and_iterm() {
        let bytes: &[u8] =
            b"\x1b]7;file:///tmp/a\x07\x1b]9;9;/tmp/b\x1b\\\x1b]1337;CurrentDir=/tmp/c\x07";
        let events = scan_chunks(&[bytes]);
        let payloads: Vec<_> = events
            .into_iter()
            .map(|event| match event.event {
                ScanEvent::WorkingDirectory(payload) => payload,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            payloads,
            vec![
                WorkingDirectoryReport::Uri(b"file:///tmp/a".to_vec()),
                WorkingDirectoryReport::Path(b"/tmp/b".to_vec()),
                WorkingDirectoryReport::Path(b"/tmp/c".to_vec())
            ]
        );
        assert_chunk_equivalence(bytes);
    }

    #[test]
    fn only_conemu_progress_counts_as_progress() {
        let bytes: &[u8] =
            b"\x1b]9;4;3;50\x07\x1b]9;build finished\x1b\\\x1b]9;4\x07\x1b]9;42\x07\x1b]9;9;/tmp\x07";
        assert_eq!(
            scanned_events(bytes),
            vec![
                ScanEvent::Progress(Progress {
                    state: ProgressState::Indeterminate,
                    percent: Some(50),
                }),
                ScanEvent::WorkingDirectory(WorkingDirectoryReport::Path(b"/tmp".to_vec())),
            ]
        );
        assert_chunk_equivalence(bytes);
    }

    #[test]
    fn progress_parses_state_and_percent_once() {
        let progress = |payload: &[u8]| Progress::parse(payload);
        assert_eq!(
            progress(b"4;3;"),
            Some(Progress {
                state: ProgressState::Indeterminate,
                percent: None,
            })
        );
        assert_eq!(
            progress(b"4;0;0"),
            Some(Progress {
                state: ProgressState::Remove,
                percent: Some(0),
            })
        );
        // Grok's busy spelling has no valid percentage.
        assert_eq!(
            progress(b"4;1;-1"),
            Some(Progress {
                state: ProgressState::Normal,
                percent: None,
            })
        );
        assert_eq!(progress(b"4"), None);
        assert_eq!(progress(b"4;"), None);
        assert_eq!(progress(b"4;9;1"), None);
        assert_eq!(progress(b"42"), None);
        let text = |payload: &[u8]| progress(payload).map(|progress| progress.to_string());
        assert_eq!(text(b"4;3;").as_deref(), Some("4;3"));
        assert_eq!(text(b"4;1;40").as_deref(), Some("4;1;40"));
        assert_eq!(text(b"4;0;0").as_deref(), Some("4;0;0"));
    }

    #[test]
    fn osc_bodies_are_emitted_only_when_captured() {
        let bytes: &[u8] = b"\x1bPignored\x1b]0;not-osc\x07\x1b\\\x1b]9;a\x1b\\\x1b]2;b\x1b[m\x1b]0;c\x18d\x1b]0;e\x01f\x07";
        let mut scanner = Scanner::default();
        assert!(scanner.scan(bytes).is_empty());

        // The DCS ends at its ESC, so the OSC after it is real. An ESC ends an
        // OSC whatever follows it, and CAN ends one too.
        let mut scanner = Scanner::default();
        scanner.set_capture_osc_bodies(true);
        let bodies: Vec<Vec<u8>> = scanner
            .scan(bytes)
            .into_iter()
            .filter_map(|event| match event.event {
                ScanEvent::OscBody(body) => Some(body),
                _ => None,
            })
            .collect();
        assert_eq!(
            bodies,
            vec![
                b"0;not-osc".to_vec(),
                b"9;a".to_vec(),
                b"2;b".to_vec(),
                b"0;c".to_vec(),
                b"0;ef".to_vec(),
            ]
        );
    }

    #[test]
    fn osc_bodies_split_across_chunks_are_whole() {
        let mut scanner = Scanner::default();
        scanner.set_capture_osc_bodies(true);
        assert!(scanner.scan(b"\x1b]21337;stat").is_empty());
        let events = scanner.scan(b"us=working\x1b\\");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].event,
            ScanEvent::OscBody(b"21337;status=working".to_vec())
        );
    }

    #[test]
    fn queries_are_recognised() {
        let bytes: &[u8] = b"\x1b[?996n\x1b[16t\x1b[14t\x1b[18t\x1b[?3J";
        assert_eq!(
            scanned_events(bytes),
            vec![
                ScanEvent::ColorSchemeQuery,
                ScanEvent::CellSizeQuery,
                ScanEvent::EraseScrollback
            ]
        );
        assert_chunk_equivalence(bytes);
    }

    #[test]
    fn only_modify_other_keys_spellings_vte_drops_are_reported() {
        let bytes: &[u8] =
            b"\x1b[>4;1m\x1b[>4;02m\x1b[>4;9m\x1b[>4m\x1b[>4;m\x1b[>m\x1b[>4;2m\x1b[>04n";
        assert_eq!(
            scanned_events(bytes),
            [
                crate::ModifyOtherKeysLevel::All,
                crate::ModifyOtherKeysLevel::Off,
                crate::ModifyOtherKeysLevel::Off
            ]
            .into_iter()
            .map(ScanEvent::ModifyOtherKeys)
            .collect::<Vec<_>>()
        );
        assert_chunk_equivalence(bytes);
    }

    #[test]
    fn unrelated_modifier_sequences_leave_modify_other_keys_alone() {
        // SGR, other XTMODKEYS resources, other DSRs and a three-part form.
        let bytes: &[u8] = b"\x1b[4;2m\x1b[>1;2m\x1b[4n\x1b[>1n\x1b[>4;2n\x1b[>4;2;1m";
        assert!(scan_chunks(&[bytes]).is_empty());
    }

    #[test]
    fn ignored_strings_do_not_leak_sequences() {
        let mut bytes = b"\x1b_Gpayload \x1b[?996n".to_vec();
        // The APC ends at ESC, so the CSI after it is still seen.
        let events = scan_chunks(&[bytes.as_slice()]);
        assert_eq!(events.len(), 1);
        bytes.extend(0..=255u8);
        assert_chunk_equivalence(&bytes);
    }
}
