//! Byte-level scanner for the few control sequences alacritty_terminal does not
//! model but shepr must answer or track: OSC 7 / OSC 9;9 / OSC 1337 CurrentDir
//! working-directory reports, DECSET/DECRST for modes 9, 1016, 2031 and 2048
//! (plus the mouse modes that cancel them), CSI ? 996 n, CSI 16 t, XTGETTCAP
//! (7-bit `ESC P + q` and raw C1 `0x90 + q`), xterm modifyOtherKeys
//! (`CSI > 4 ; Pv m`, `CSI > 4 n`), RIS, `CSI ? 3 J`, and the halfwidth katakana voiced
//! sound marks U+FF9E/U+FF9F in ground state (see [`ScanEvent::HalfwidthVoicedMark`]).
//!
//! The scanner mirrors vte's framing rules closely enough that it agrees with
//! the core about where each sequence ends: OSC ends on BEL, ESC, CAN or SUB;
//! DCS passthrough ends on ESC, C1 ST, CAN or SUB; SOS/PM/APC strings end on
//! ESC, CAN or SUB. Raw C1 bytes are only recognised outside UTF-8 sequences.
//! Events carry the offset just past the byte that completed them, relative to
//! the slice handed to [`Scanner::scan`], so callers can interleave the core's
//! own replies with ours in byte order.

const MAX_CSI_BYTES: usize = 64;
const MAX_OSC_BYTES: usize = 4096;
const MAX_DCS_INTRO_BYTES: usize = 16;
const MAX_XTGETTCAP_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ScanEvent {
    /// DECSET (`enabled`) or DECRST of a private mode shepr tracks itself.
    PrivateMode { mode: u16, enabled: bool },
    /// CSI ? 996 n (color scheme DSR).
    ColorSchemeQuery,
    /// CSI 16 t (cell size in pixels).
    CellSizeQuery,
    /// Complete XTGETTCAP replies, in request order.
    Xtgettcap(Vec<Vec<u8>>),
    /// Working-directory report payload (URI or path, exactly as sent).
    WorkingDirectory(Vec<u8>),
    /// RIS (ESC c).
    FullReset,
    /// CSI ? 3 J: the DECSED spelling of ED3 (erase scrollback). vte only
    /// dispatches `CSI 3 J`, but programs (Droid among them) emit this form.
    EraseScrollback,
    /// xterm modifyOtherKeys level (0, 1 or 2) set by `CSI > 4 ; Pv m`,
    /// `CSI > m` or `CSI > 4 n`.
    ModifyOtherKeys(u8),
    /// U+FF9E or U+FF9F printed in ground state; `end` is just past its last
    /// UTF-8 byte. unicode-width gives these Grapheme_Extend marks width 0, but
    /// wcwidth (and so the programs in the pane) gives them a column of their own.
    HalfwidthVoicedMark(char),
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
    CsiIgnore,
    Osc,
    DcsIntro,
    XtgettcapBody,
    DcsIgnore,
    StringIgnore,
}

#[derive(Debug, Default)]
pub(super) struct Scanner {
    state: State,
    utf8_remaining: u8,
    utf8_codepoint: u32,
    buffer: Vec<u8>,
    overflow: bool,
}

/// Private modes whose DECSET/DECRST the adapter must observe. 9, 1016, 2031
/// and 2048 are modelled by the adapter; 1000/1002/1003 cancel X10 (9), and
/// 1005 cancels SGR-pixels (1016). Re-asserting 1006 deliberately does not:
/// apps resend it after 1016 and still expect pixel coordinates.
const TRACKED_PRIVATE_MODES: &[u16] = &[9, 1000, 1002, 1003, 1005, 1016, 2031, 2048];

impl Scanner {
    pub(super) fn scan(&mut self, bytes: &[u8]) -> Vec<ScannedEvent> {
        let mut events = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            if self.state == State::Ground && self.utf8_remaining == 0 {
                // Fast path: plain ASCII text cannot start anything we track.
                match bytes[index..]
                    .iter()
                    .position(|&byte| byte == 0x1b || byte >= 0x80)
                {
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

    /// How many trailing bytes of the input scanned so far are the start of a
    /// U+FF9E/U+FF9F (`EF` or `EF BE`) still waiting for its final byte.
    pub(super) fn voiced_mark_prefix_len(&self) -> usize {
        if self.state != State::Ground {
            return 0;
        }
        match (self.utf8_remaining, self.utf8_codepoint) {
            (2, 0x0f) => 1,
            (1, 0x3fe) => 2,
            _ => 0,
        }
    }

    fn step(&mut self, byte: u8, index: usize, events: &mut Vec<ScannedEvent>) {
        match self.state {
            State::Ground => self.ground(byte, index, events),
            State::Escape => self.escape(byte, index, events),
            State::EscapeIntermediate => match byte {
                0x1b => self.enter(State::Escape),
                0x18 | 0x1a => self.enter(State::Ground),
                0x30..=0x7e => self.enter(State::Ground),
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
                // change the sequence; everything else is ignored.
                _ => {}
            },
            State::CsiIgnore => match byte {
                0x1b => self.enter(State::Escape),
                0x18 | 0x1a => self.enter(State::Ground),
                0x40..=0x7e => self.enter(State::Ground),
                _ => {}
            },
            State::Osc => match byte {
                0x07 => {
                    self.dispatch_osc(index, events);
                    self.enter(State::Ground);
                }
                0x18 | 0x1a => {
                    self.dispatch_osc(index, events);
                    self.enter(State::Ground);
                }
                0x1b => {
                    self.dispatch_osc(index, events);
                    self.enter(State::Escape);
                }
                0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1c..=0x1f => {}
                _ => {
                    if self.buffer.len() >= MAX_OSC_BYTES {
                        self.overflow = true;
                    } else {
                        self.buffer.push(byte);
                    }
                }
            },
            State::DcsIntro => match byte {
                0x1b => self.enter(State::Escape),
                0x18 | 0x1a | 0x9c => self.enter(State::Ground),
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
                _ => {
                    if self.buffer.len() >= MAX_XTGETTCAP_BYTES {
                        self.overflow = true;
                    } else {
                        self.buffer.push(byte);
                    }
                }
            },
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

    fn ground(&mut self, byte: u8, index: usize, events: &mut Vec<ScannedEvent>) {
        if byte < 0x80 {
            self.utf8_remaining = 0;
            if byte == 0x1b {
                self.enter(State::Escape);
            }
            return;
        }
        if self.utf8_remaining > 0 && (0x80..=0xbf).contains(&byte) {
            self.utf8_remaining -= 1;
            self.utf8_codepoint = (self.utf8_codepoint << 6) | u32::from(byte & 0x3f);
            if self.utf8_remaining == 0
                && let Some(mark) = halfwidth_voiced_mark(self.utf8_codepoint)
            {
                events.push(ScannedEvent {
                    end: index + 1,
                    event: ScanEvent::HalfwidthVoicedMark(mark),
                });
            }
            return;
        }
        (self.utf8_remaining, self.utf8_codepoint) = match byte {
            0xc2..=0xdf => (1, u32::from(byte & 0x1f)),
            0xe0..=0xef => (2, u32::from(byte & 0x0f)),
            0xf0..=0xf4 => (3, u32::from(byte & 0x07)),
            _ => (0, 0),
        };
        if byte == 0x90 {
            // Legacy raw C1 DCS. The core ignores it, but XTGETTCAP clients
            // using 8-bit controls still expect a reply.
            self.enter(State::DcsIntro);
        }
    }

    fn escape(&mut self, byte: u8, index: usize, events: &mut Vec<ScannedEvent>) {
        match byte {
            0x18 | 0x1a => self.enter(State::Ground),
            // ESC ESC restarts the escape; other C0 controls execute in place.
            0x00..=0x1f => {}
            0x20..=0x2f => self.enter(State::EscapeIntermediate),
            b'[' => self.enter(State::Csi),
            b']' => self.enter(State::Osc),
            b'P' => self.enter(State::DcsIntro),
            b'X' | b'^' | b'_' => self.enter(State::StringIgnore),
            b'c' => {
                events.push(ScannedEvent {
                    end: index + 1,
                    event: ScanEvent::FullReset,
                });
                self.enter(State::Ground);
            }
            0x30..=0x7e => self.enter(State::Ground),
            // vte ignores DEL and high bytes while an escape is pending.
            _ => {}
        }
    }

    fn enter(&mut self, state: State) {
        self.state = state;
        self.buffer.clear();
        self.overflow = false;
        if state != State::Ground {
            self.utf8_remaining = 0;
        }
    }

    fn dispatch_csi(&mut self, final_byte: u8, index: usize, events: &mut Vec<ScannedEvent>) {
        let params = self.buffer.as_slice();
        match final_byte {
            b'h' | b'l' => {
                let Some(modes) = params.strip_prefix(b"?") else {
                    return;
                };
                if modes
                    .iter()
                    .any(|byte| !byte.is_ascii_digit() && *byte != b';')
                {
                    return;
                }
                for mode in modes.split(|byte| *byte == b';') {
                    let Some(mode) = parse_decimal(mode) else {
                        continue;
                    };
                    if TRACKED_PRIVATE_MODES.contains(&mode) {
                        events.push(ScannedEvent {
                            end: index + 1,
                            event: ScanEvent::PrivateMode {
                                mode,
                                enabled: final_byte == b'h',
                            },
                        });
                    }
                }
            }
            b'n' if params == b"?996" => events.push(ScannedEvent {
                end: index + 1,
                event: ScanEvent::ColorSchemeQuery,
            }),
            b'J' if params == b"?3" => events.push(ScannedEvent {
                end: index + 1,
                event: ScanEvent::EraseScrollback,
            }),
            // CSI > 4 n: modifyOtherKeys back to its default (off).
            b'n' if params
                .strip_prefix(b">")
                .is_some_and(|resource| parse_decimal(resource) == Some(4)) =>
            {
                events.push(ScannedEvent {
                    end: index + 1,
                    event: ScanEvent::ModifyOtherKeys(0),
                });
            }
            b'm' => {
                if let Some(level) = params.strip_prefix(b">").and_then(modify_other_keys_level) {
                    events.push(ScannedEvent {
                        end: index + 1,
                        event: ScanEvent::ModifyOtherKeys(level),
                    });
                }
            }
            b't' => {
                let first = params
                    .split(|byte| *byte == b';')
                    .next()
                    .unwrap_or_default();
                if first.iter().all(u8::is_ascii_digit) && parse_decimal(first) == Some(16) {
                    events.push(ScannedEvent {
                        end: index + 1,
                        event: ScanEvent::CellSizeQuery,
                    });
                }
            }
            _ => {}
        }
    }

    fn dispatch_osc(&mut self, index: usize, events: &mut Vec<ScannedEvent>) {
        if self.overflow {
            return;
        }
        let body = self.buffer.as_slice();
        let payload = body
            .strip_prefix(b"7;")
            .or_else(|| body.strip_prefix(b"9;9;"))
            .or_else(|| body.strip_prefix(b"1337;CurrentDir="));
        if let Some(payload) = payload {
            events.push(ScannedEvent {
                end: index + 1,
                event: ScanEvent::WorkingDirectory(payload.to_vec()),
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

/// The modifyOtherKeys level set by `CSI > params m` (XTMODKEYS); `None` when
/// the sequence addresses another resource. A bare `CSI > m` resets it.
fn modify_other_keys_level(params: &[u8]) -> Option<u8> {
    if params.is_empty() {
        return Some(0);
    }
    let mut parts = params.split(|byte| *byte == b';');
    let resource = parts.next().unwrap_or_default();
    let value = parts.next();
    if parts.next().is_some() || parse_decimal(resource) != Some(4) {
        return None;
    }
    let level = value.and_then(parse_decimal).unwrap_or(0).min(2);
    Some(u8::try_from(level).unwrap_or(2))
}

fn halfwidth_voiced_mark(codepoint: u32) -> Option<char> {
    matches!(codepoint, 0xff9e | 0xff9f)
        .then(|| char::from_u32(codepoint))
        .flatten()
}

fn parse_decimal(bytes: &[u8]) -> Option<u16> {
    if bytes.is_empty() || bytes.len() > 5 {
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
        b"544E" => Some(Some(crate::pane::PANE_TERM.as_bytes())),
        // Co / colors: palette size.
        b"436F" => Some(Some(b"256")),
        b"636F6C6F7273" => Some(Some(b"256")),
        // Tc: truecolor boolean.
        b"5463" => Some(None),
        // RGB: bits per channel.
        b"524742" => Some(Some(b"8")),
        // setrgbf / setrgbb.
        b"73657472676266" => Some(Some(b"\\E[38:2:%p1%d:%p2%d:%p3%dm")),
        b"73657472676262" => Some(Some(b"\\E[48:2:%p1%d:%p2%d:%p3%dm")),
        // Ms: OSC 52 clipboard.
        b"4D73" => Some(Some(b"\\E]52;%p1%s;%p2%s\\007")),
        // Su: styled underlines boolean.
        b"5375" => Some(None),
        // Smulx: underline style.
        b"536D756C78" => Some(Some(b"\\E[4:%p1%dm")),
        // Setulc: underline color.
        b"536574756C63" => Some(Some(
            b"\\E[58:2::%p1%{65536}%/%d:%p1%{256}%/%{255}%&%d:%p1%{255}%&%d%;m",
        )),
        _ => None,
    }
}

fn build_xtgettcap_response(cap_hex: &[u8], value: Option<&[u8]>) -> Vec<u8> {
    let mut response =
        Vec::with_capacity(8 + cap_hex.len() + value.map_or(0, |bytes| bytes.len() * 2));
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

    #[test]
    fn tracked_private_modes_are_reported_at_their_final_byte() {
        let bytes: &[u8] = b"x\x1b[?1016;2031h\x1b[?25l\x1b[?9l";
        let events = scan_chunks(&[bytes]);
        assert_eq!(
            events,
            vec![
                ScannedEvent {
                    end: 14,
                    event: ScanEvent::PrivateMode {
                        mode: 1016,
                        enabled: true
                    }
                },
                ScannedEvent {
                    end: 14,
                    event: ScanEvent::PrivateMode {
                        mode: 2031,
                        enabled: true
                    }
                },
                ScannedEvent {
                    end: bytes.len(),
                    event: ScanEvent::PrivateMode {
                        mode: 9,
                        enabled: false
                    }
                },
            ]
        );
        assert_chunk_equivalence(bytes);
    }

    #[test]
    fn xtgettcap_replies_for_7bit_and_c1_intros() {
        for bytes in [
            b"\x1bP+q5463;524742\x1b\\".as_slice(),
            b"\x90+q5463;524742\x9c".as_slice(),
            b"\x90+q5463;524742\x1b\\".as_slice(),
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
    fn xtgettcap_completes_at_escape_like_the_core() {
        let bytes: &[u8] = b"\x1bP+q4d73\x1b\\";
        let events = scan_chunks(&[bytes]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].end, bytes.len() - 1);
    }

    #[test]
    fn unknown_xtgettcap_names_are_silent() {
        assert!(scan_chunks(&[b"\x1bP+q6E6F7065;4D7\x1b\\".as_slice()]).is_empty());
    }

    #[test]
    fn c1_bytes_inside_utf8_are_text() {
        // U+00D0 is C3 90 and U+00DC is C3 9C: neither may open or close a DCS.
        let mut bytes = "\u{d0}+q5463\u{dc}".as_bytes().to_vec();
        bytes.extend_from_slice(b"\x1b[?2031h");
        let events = scan_chunks(&[bytes.as_slice()]);
        assert_eq!(
            events,
            vec![ScannedEvent {
                end: bytes.len(),
                event: ScanEvent::PrivateMode {
                    mode: 2031,
                    enabled: true
                }
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
                b"file:///tmp/a".to_vec(),
                b"/tmp/b".to_vec(),
                b"/tmp/c".to_vec()
            ]
        );
        assert_chunk_equivalence(bytes);
    }

    #[test]
    fn queries_and_reset_are_recognised() {
        let bytes: &[u8] = b"\x1b[?996n\x1b[16t\x1b[14t\x1bc";
        let events: Vec<_> = scan_chunks(&[bytes])
            .into_iter()
            .map(|event| event.event)
            .collect();
        assert_eq!(
            events,
            vec![
                ScanEvent::ColorSchemeQuery,
                ScanEvent::CellSizeQuery,
                ScanEvent::FullReset
            ]
        );
        assert_chunk_equivalence(bytes);
    }

    #[test]
    fn modify_other_keys_levels_are_reported() {
        let bytes: &[u8] =
            b"\x1b[>4;1m\x1b[>4;02m\x1b[>4;9m\x1b[>4m\x1b[>4;2m\x1b[>m\x1b[>4;2m\x1b[>04n";
        let levels: Vec<_> = scan_chunks(&[bytes])
            .into_iter()
            .map(|event| event.event)
            .collect();
        assert_eq!(
            levels,
            [1, 2, 2, 0, 2, 0, 2, 0]
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
    fn halfwidth_voiced_marks_are_reported_only_in_ground_text() {
        let text = "\u{ff76}\u{ff9e}\u{ff8a}\u{ff9f}\u{ff9d}";
        let mut bytes = text.as_bytes().to_vec();
        // Inside an OSC the mark is payload, not text.
        bytes.extend_from_slice("\x1b]0;\u{ff9e}\x07".as_bytes());
        let events = scan_chunks(&[bytes.as_slice()]);
        assert_eq!(
            events,
            vec![
                ScannedEvent {
                    end: 6,
                    event: ScanEvent::HalfwidthVoicedMark('\u{ff9e}')
                },
                ScannedEvent {
                    end: 12,
                    event: ScanEvent::HalfwidthVoicedMark('\u{ff9f}')
                },
            ]
        );
        assert_chunk_equivalence(&bytes);
    }

    #[test]
    fn ignored_strings_do_not_leak_sequences() {
        let mut bytes = b"\x1b_Gpayload \x1b[?2031h".to_vec();
        // The APC ends at ESC, so the CSI after it is still seen.
        let events = scan_chunks(&[bytes.as_slice()]);
        assert_eq!(events.len(), 1);
        bytes.extend(0..=255u8);
        assert_chunk_equivalence(&bytes);
    }
}
