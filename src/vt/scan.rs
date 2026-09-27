//! Byte-level scanner for the few control sequences vte never hands to a
//! `Handler`, but shepr must answer or track: OSC 7 / OSC 9;9 / OSC 1337
//! CurrentDir working-directory reports, OSC 9;4 progress (agent detection
//! evidence), CSI ? 996 n, CSI 16 t, XTGETTCAP
//! (`ESC P + q`), `CSI ? 3 J`, and the modifyOtherKeys spellings vte drops
//! (`CSI > m`, `CSI > 4 n`, `CSI > 4 ; Pv m` with Pv above 2).
//!
//! Everything vte does dispatch (private modes, DECRQM, RIS, the vte-parsed
//! modifyOtherKeys forms, printed characters) is handled by the parser's
//! `Handler` wrapper in `handler.rs` instead, where it stays in byte order
//! inside synchronized updates too. Do not add sequences here that vte
//! dispatches.
//!
//! The scanner mirrors vte's framing so it agrees with the core about where
//! each sequence starts and ends. vte only understands 7-bit controls: in
//! ground state every byte other than ESC is text or a no-op control (raw C1
//! bytes such as 0x90 are executed as no-ops, never open a sequence), so the
//! scanner only leaves ground on ESC. Inside sequences: OSC ends on BEL, ESC,
//! CAN or SUB; a DCS body ends on ESC, CAN, SUB or a raw 0x9C (vte's only
//! 8-bit control, and only in DCS passthrough); SOS/PM/APC strings end on ESC,
//! CAN or SUB. Events carry the offset just past the byte that completed them,
//! relative to the slice handed to [`Scanner::scan`], so callers can
//! interleave the core's own replies with ours in byte order.

const MAX_CSI_BYTES: usize = 64;
const MAX_OSC_BYTES: usize = 4096;
const MAX_DCS_INTRO_BYTES: usize = 16;
const MAX_XTGETTCAP_BYTES: usize = 1024;

/// Raw OSC working-directory report. It may be a URI or a path, so parsing
/// belongs to the pane after the terminal scanner has framed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkingDirectoryReport(pub(crate) Vec<u8>);

/// Raw ConEmu OSC 9;4 payload, including its `4` command byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProgressReport(pub(crate) Vec<u8>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ScanEvent {
    /// CSI ? 996 n (color scheme DSR).
    ColorSchemeQuery,
    /// CSI 16 t (cell size in pixels).
    CellSizeQuery,
    /// Complete XTGETTCAP replies, in request order.
    Xtgettcap(Vec<Vec<u8>>),
    /// Working-directory report payload (URI or path, exactly as sent).
    WorkingDirectory(WorkingDirectoryReport),
    /// ConEmu progress report: the OSC 9 payload after `9;`, starting `4`.
    Progress(ProgressReport),
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
}

impl Scanner {
    pub(super) fn scan(&mut self, bytes: &[u8]) -> Vec<ScannedEvent> {
        let mut events = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            if self.state == State::Ground {
                // Fast path: only ESC leaves ground state.
                match bytes[index..].iter().position(|&byte| byte == 0x1b) {
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
                // change the sequence; DEL and high bytes are ignored.
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
                _ => {
                    if self.buffer.len() >= MAX_XTGETTCAP_BYTES {
                        self.overflow = true;
                    } else {
                        self.buffer.push(byte);
                    }
                }
            },
            // vte's DCS passthrough for every other DCS.
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
            0x18 | 0x1a => self.enter(State::Ground),
            // ESC ESC restarts the escape; other C0 controls execute in place.
            0x00..=0x1f => {}
            0x20..=0x2f => self.enter(State::EscapeIntermediate),
            b'[' => self.enter(State::Csi),
            b']' => self.enter(State::Osc),
            b'P' => self.enter(State::DcsIntro),
            b'X' | b'^' | b'_' => self.enter(State::StringIgnore),
            0x30..=0x7e => self.enter(State::Ground),
            // vte ignores DEL and high bytes while an escape is pending.
            _ => {}
        }
    }

    fn enter(&mut self, state: State) {
        self.state = state;
        self.buffer.clear();
        self.overflow = false;
    }

    fn dispatch_csi(&mut self, final_byte: u8, index: usize, events: &mut Vec<ScannedEvent>) {
        let params = self.buffer.as_slice();
        let event = match final_byte {
            b'n' if params == b"?996" => Some(ScanEvent::ColorSchemeQuery),
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
        let payload = body
            .strip_prefix(b"7;")
            .or_else(|| body.strip_prefix(b"9;9;"))
            .or_else(|| body.strip_prefix(b"1337;CurrentDir="));
        if let Some(payload) = payload {
            events.push(ScannedEvent {
                end: index + 1,
                event: ScanEvent::WorkingDirectory(WorkingDirectoryReport(payload.to_vec())),
            });
            return;
        }
        // ConEmu progress is `OSC 9 ; 4 ; state ; percent`. Every other OSC 9
        // is an iTerm2-style notification (or ConEmu's other subcommands),
        // which must not overwrite progress evidence.
        if let Some(payload) = body
            .strip_prefix(b"9;")
            .filter(|payload| *payload == b"4" || payload.starts_with(b"4;"))
        {
            events.push(ScannedEvent {
                end: index + 1,
                event: ScanEvent::Progress(ProgressReport(payload.to_vec())),
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
                WorkingDirectoryReport(b"file:///tmp/a".to_vec()),
                WorkingDirectoryReport(b"/tmp/b".to_vec()),
                WorkingDirectoryReport(b"/tmp/c".to_vec())
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
                ScanEvent::Progress(ProgressReport(b"4;3;50".to_vec())),
                ScanEvent::Progress(ProgressReport(b"4".to_vec())),
                ScanEvent::WorkingDirectory(WorkingDirectoryReport(b"/tmp".to_vec())),
            ]
        );
        assert_chunk_equivalence(bytes);
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
                crate::vt::ModifyOtherKeysLevel::All,
                crate::vt::ModifyOtherKeysLevel::Off,
                crate::vt::ModifyOtherKeysLevel::Off
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
