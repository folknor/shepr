use crossterm::event::{KeyCode, KeyModifiers};
use std::fmt::Write as _;

use super::TerminalKey;
use super::tables::{control_byte, functional_key, modifier_bits};
use crate::limits::KITTY_KEY_SEQUENCE_INITIAL_CAPACITY;
use crate::{KittyKeyboardFlags, ModifyOtherKeysLevel};
use shepr_core::limits::UTF8_MAX_BYTES_PER_CODEPOINT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyboardProtocol(KeyboardProtocolMode);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyboardProtocolMode {
    Legacy,
    Kitty(KittyKeyboardFlags),
}

impl KeyboardProtocol {
    pub const fn legacy() -> Self {
        Self(KeyboardProtocolMode::Legacy)
    }

    pub const fn from_flags(flags: KittyKeyboardFlags) -> Self {
        if flags.is_empty() {
            Self::legacy()
        } else {
            Self(KeyboardProtocolMode::Kitty(flags))
        }
    }

    pub const fn is_kitty(self) -> bool {
        matches!(self.0, KeyboardProtocolMode::Kitty(_))
    }

    pub const fn kitty_flags(self) -> KittyKeyboardFlags {
        match self.0 {
            KeyboardProtocolMode::Legacy => KittyKeyboardFlags::NONE,
            KeyboardProtocolMode::Kitty(flags) => flags,
        }
    }

    pub fn reports_event_types(self) -> bool {
        self.kitty_flags()
            .contains(KittyKeyboardFlags::REPORT_EVENT_TYPES)
    }

    pub fn reports_all_keys(self) -> bool {
        self.kitty_flags()
            .contains(KittyKeyboardFlags::REPORT_ALL_KEYS)
    }
}

pub fn encode_terminal_key(mut key: TerminalKey, protocol: KeyboardProtocol) -> Vec<u8> {
    normalize_backtab_key(&mut key, protocol.is_kitty());
    let flags = protocol.kitty_flags();
    // Legacy encoding has no Super modifier bit. Preserve the chord with CSI-u
    // instead of leaking the unmodified key into the pane.
    if !protocol.is_kitty()
        && key.kind != crossterm::event::KeyEventKind::Release
        && key.modifiers.contains(KeyModifiers::SUPER)
        && let Some(bytes) = try_encode_csi_u(&key, KittyKeyboardFlags::NONE)
    {
        return bytes;
    }

    // Text the client committed is sent as that text, except under kitty
    // REPORT_ALL_KEYS: that mode reports every key, text-producing ones
    // included, as an escape code and sends no plain text at all. The child
    // gets the text back only through REPORT_ASSOCIATED_TEXT, which the CSI u
    // encoder below fills from the committed text.
    if key.kind != crossterm::event::KeyEventKind::Release
        && !protocol.reports_all_keys()
        && let Some(text) = &key.generated_text
    {
        return text.as_bytes().to_vec();
    }

    // A release event only produces bytes when the pane protocol reports event
    // types (Kitty REPORT_EVENT_TYPES). Otherwise the child expects a single
    // legacy byte per keystroke, so re-emitting it on release would double keys
    // like Enter/Backspace. Release events can reach this fallback encoder,
    // so it must enforce the same event-type gate.
    if key.kind == crossterm::event::KeyEventKind::Release && !protocol.reports_event_types() {
        return Vec::new();
    }

    let kitty_first = protocol.reports_all_keys()
        || (key.kind == crossterm::event::KeyEventKind::Release && protocol.reports_event_types());

    if kitty_first
        && protocol.is_kitty()
        && let Some(bytes) = try_encode_csi_u(&key, flags)
    {
        return bytes;
    }

    // A committed key the CSI u encoder has no form for keeps its text rather
    // than being dropped.
    if key.kind != crossterm::event::KeyEventKind::Release
        && let Some(text) = &key.generated_text
    {
        return text.as_bytes().to_vec();
    }

    if let Some(bytes) = encode_text_input(&key) {
        return bytes;
    }

    if !kitty_first
        && protocol.is_kitty()
        && let Some(bytes) = try_encode_csi_u(&key, flags)
    {
        return bytes;
    }
    if key.kind == crossterm::event::KeyEventKind::Release && protocol.reports_event_types() {
        return Vec::new();
    }
    encode_legacy(key)
}

/// CSI u encoding: \e[{codepoint};{modifiers}u
/// Used when the child has pushed Kitty keyboard enhancement.
/// Returns None if the key doesn't need CSI u (unmodified basic keys).
fn try_encode_csi_u(key: &TerminalKey, flags: KittyKeyboardFlags) -> Option<Vec<u8>> {
    let mods = key.modifiers;
    let event_suffix = kitty_event_suffix(key, flags);
    let disambiguate = flags.contains(KittyKeyboardFlags::DISAMBIGUATE);
    let report_all_keys = flags.contains(KittyKeyboardFlags::REPORT_ALL_KEYS);
    let reports_non_press_event = key.kind != crossterm::event::KeyEventKind::Press
        && flags.contains(KittyKeyboardFlags::REPORT_EVENT_TYPES);

    // Alternate-key reporting only decorates an escape code selected for some
    // other reason, and event-type reporting only needs a new encoding for
    // repeats and releases. Keep character chords with an existing legacy
    // spelling on that spelling unless disambiguation or report-all requests
    // CSI u.
    if !disambiguate && !report_all_keys && !reports_non_press_event && kitty_legacy_text_key(key) {
        return None;
    }

    if !report_all_keys
        && key.modifiers.is_empty()
        && matches!(key.code, KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace)
    {
        return None;
    }

    // Unmodified keys use legacy encoding (more compatible)
    if mods.is_empty() && event_suffix.is_none() && !report_all_keys {
        return None;
    }

    // Special keys (arrows, F-keys, etc.) have well-established legacy
    // xterm modified formats (\x1b[1;3A for Alt+Up, etc.) that are universally
    // understood. Even Ghostty sends these in legacy format with kitty mode on.
    // Use CSI u for character keys and keys without legacy forms. Super chords
    // on functional keys also need it because xterm's modifier bits omit Super.
    if (functional_key(key.code).is_some() || matches!(key.code, KeyCode::F(_)))
        && event_suffix.is_none()
        && !report_all_keys
        && !mods.contains(KeyModifiers::SUPER)
    {
        return None;
    }

    let (codepoint, alternate_shifted) = match key.code {
        KeyCode::Char(c) => {
            let base = canonical_kitty_char(c, mods);
            let shifted = alternate_shifted_codepoint(key, flags);
            (base as u32, shifted)
        }
        KeyCode::Enter => (13, None),
        KeyCode::Tab => (9, None),
        KeyCode::Backspace => (127, None),
        KeyCode::Esc => (27, None),
        // The main-block navigation and function keys keep their legacy
        // `CSI 1;mods X` / `CSI n;mods ~` forms in the kitty protocol. The
        // 57417.. codepoints belong to the *keypad* variants of these keys.
        code => return encode_kitty_functional_key(code, mods, event_suffix),
    };

    let modifier = kitty_modifier(mods);

    let mut sequence = String::with_capacity(KITTY_KEY_SEQUENCE_INITIAL_CAPACITY);
    sequence.push_str("\x1b[");
    write!(&mut sequence, "{codepoint}").ok()?;
    if let Some(shifted) = alternate_shifted {
        write!(&mut sequence, ":{}", u32::from(shifted)).ok()?;
    }
    write!(&mut sequence, ";{modifier}").ok()?;
    if let Some(event) = event_suffix {
        write!(&mut sequence, ":{event}").ok()?;
    }
    // Associated text depends on REPORT_ALL_KEYS; the spec says the flag is
    // ignored without it.
    if report_all_keys && flags.contains(KittyKeyboardFlags::REPORT_ASSOCIATED_TEXT) {
        write_associated_text(&mut sequence, key).ok()?;
    }
    sequence.push('u');

    Some(sequence.into_bytes())
}

/// Kitty encoding of arrows, Home/End, Insert/Delete, PageUp/PageDown and
/// F1-F12: `CSI 1;mods[:event] {A,B,C,D,H,F,P,Q,S}` or
/// `CSI n;mods[:event] ~` (F3 is `CSI 13~` so it cannot be mistaken for a
/// cursor position report).
/// Full report-all support for F13+, lock, media and bare modifier keys is not
/// needed by agent and shell panes.
fn encode_kitty_functional_key(
    code: KeyCode,
    mods: KeyModifiers,
    event_suffix: Option<u8>,
) -> Option<Vec<u8>> {
    let functional = functional_key(code)?;
    // CSI 1;mods R is ambiguous with cursor position reports in kitty mode.
    let (number, final_byte) = if code == KeyCode::F(3) {
        (13, '~')
    } else {
        (functional.number, functional.final_byte)
    };
    let modifier = kitty_modifier(mods);
    let mut sequence = format!("\x1b[{number};{modifier}");
    if let Some(event) = event_suffix {
        write!(&mut sequence, ":{event}").ok()?;
    }
    sequence.push(final_byte);
    Some(sequence.into_bytes())
}

/// Pane state that selects how a non-text key is encoded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyEncodeModes {
    /// Active kitty keyboard flags; an empty value selects legacy key encoding.
    pub kitty_flags: KittyKeyboardFlags,
    /// xterm modifyOtherKeys level. Encoding covers modified Enter
    /// and Escape, plus Tab and Backspace at level 2; agent and shell panes do
    /// not need broader level 1/2 support.
    pub modify_other_keys: ModifyOtherKeysLevel,
    /// DECCKM (mode 1): unmodified cursor keys use SS3.
    pub application_cursor: bool,
}

impl KeyEncodeModes {
    fn modify_other_keys_code(self, key: KeyCode) -> Option<u8> {
        match (self.modify_other_keys, key) {
            (
                ModifyOtherKeysLevel::ExceptWellDefined | ModifyOtherKeysLevel::All,
                KeyCode::Enter,
            ) => Some(13),
            (ModifyOtherKeysLevel::ExceptWellDefined | ModifyOtherKeysLevel::All, KeyCode::Esc) => {
                Some(27)
            }
            (ModifyOtherKeysLevel::All, KeyCode::Tab) => Some(9),
            (ModifyOtherKeysLevel::All, KeyCode::Backspace) => Some(127),
            _ => None,
        }
    }
}

/// Encode a non-text key (Enter, Tab, arrows, function keys, ...) the way the
/// child negotiated: kitty flags first, then modifyOtherKeys, then legacy
/// xterm sequences honouring application cursor mode.
pub fn encode_terminal_key_with_modes(mut key: TerminalKey, modes: KeyEncodeModes) -> Vec<u8> {
    normalize_backtab_key(
        &mut key,
        !modes.kitty_flags.is_empty() || modes.modify_other_keys == ModifyOtherKeysLevel::All,
    );
    if !modes.kitty_flags.is_empty() {
        // Disambiguation makes a bare Escape press unambiguous as CSI 27 u.
        if key.code == KeyCode::Esc
            && key.modifiers.is_empty()
            && key.kind != crossterm::event::KeyEventKind::Release
            && modes.kitty_flags.contains(KittyKeyboardFlags::DISAMBIGUATE)
            && !modes
                .kitty_flags
                .contains(KittyKeyboardFlags::REPORT_EVENT_TYPES)
            && !modes
                .kitty_flags
                .contains(KittyKeyboardFlags::REPORT_ALL_KEYS)
        {
            return b"\x1b[27u".to_vec();
        }
        let bytes =
            encode_terminal_key(key.clone(), KeyboardProtocol::from_flags(modes.kitty_flags));
        return apply_application_cursor(bytes, &key, modes.application_cursor);
    }

    if key.kind != crossterm::event::KeyEventKind::Release {
        if let Some(bytes) = encode_modify_other_keys(&key, modes) {
            return bytes;
        }
        // xterm sends ^H for Ctrl+Backspace (DEL stays plain Backspace).
        let non_alt = key.modifiers.difference(KeyModifiers::ALT);
        if key.code == KeyCode::Backspace && non_alt == KeyModifiers::CONTROL {
            return if key.modifiers.contains(KeyModifiers::ALT) {
                vec![0x1b, 0x08]
            } else {
                vec![0x08]
            };
        }
    }

    let bytes = encode_terminal_key(key.clone(), KeyboardProtocol::legacy());
    apply_application_cursor(bytes, &key, modes.application_cursor)
}

fn normalize_backtab_key(key: &mut TerminalKey, kitty_enabled: bool) {
    if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
        let canonical = key.canonical_key();
        let (canonical_code, canonical_modifiers) = (canonical.code(), canonical.modifiers());
        if canonical_code == KeyCode::BackTab && kitty_enabled {
            // Enhanced keyboard protocols encode Backtab as Tab with Shift.
            key.code = KeyCode::Tab;
            key.modifiers = canonical_modifiers | KeyModifiers::SHIFT;
        } else {
            key.code = canonical_code;
            key.modifiers = canonical_modifiers;
        }
    }
}

/// `CSI 27 ; mods ; code ~` for modified Enter/Tab/Backspace/Escape. Level 1
/// leaves Alt-only chords and Tab/Backspace (keys with well-known legacy
/// meanings) to the legacy encoder, as xterm does.
/// This limited key set is deliberate because agent and shell panes do not need
/// full modifyOtherKeys level 1/2 encoding.
fn encode_modify_other_keys(key: &TerminalKey, modes: KeyEncodeModes) -> Option<Vec<u8>> {
    let code = modes.modify_other_keys_code(key.code)?;
    let mods = key.modifiers
        & (KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::SUPER);
    if mods.is_empty()
        || (modes.modify_other_keys == ModifyOtherKeysLevel::ExceptWellDefined
            && mods == KeyModifiers::ALT)
    {
        return None;
    }
    Some(format!("\x1b[27;{};{code}~", kitty_modifier(mods)).into_bytes())
}

/// Rewrites an unmodified `CSI A/B/C/D/H/F` into its SS3 form under DECCKM.
fn apply_application_cursor(
    bytes: Vec<u8>,
    key: &TerminalKey,
    application_cursor: bool,
) -> Vec<u8> {
    if !application_cursor
        || !key.modifiers.is_empty()
        || key.kind == crossterm::event::KeyEventKind::Release
    {
        return bytes;
    }
    let Some(functional) = functional_key(key.code) else {
        return bytes;
    };
    if functional.number != 1 || !matches!(functional.final_byte, 'A' | 'B' | 'C' | 'D' | 'H' | 'F')
    {
        return bytes;
    }
    let final_byte = functional.final_byte as u8;
    if bytes.as_slice() == [0x1b, b'[', final_byte] {
        vec![0x1b, b'O', final_byte]
    } else {
        bytes
    }
}

fn text_codepoint_for_key(key: &TerminalKey) -> Option<u32> {
    let ch = text_char_for_key(key)?;
    (!ch.is_control()).then_some(ch as u32)
}

/// The kitty associated-text field, `;cp[:cp...]`: the text the key
/// produced, as codepoints, with control characters left out (the spec
/// forbids them). Text the client committed wins over text inferred from the
/// key. Releases carry no text; nothing is written when there is none.
fn write_associated_text(sequence: &mut String, key: &TerminalKey) -> std::fmt::Result {
    if key.kind == crossterm::event::KeyEventKind::Release {
        return Ok(());
    }
    let Some(text) = &key.generated_text else {
        if let Some(codepoint) = text_codepoint_for_key(key) {
            write!(sequence, ";{codepoint}")?;
        }
        return Ok(());
    };
    let mut separator = ';';
    for ch in text.chars().filter(|ch| !ch.is_control()) {
        write!(sequence, "{separator}{}", u32::from(ch))?;
        separator = ':';
    }
    Ok(())
}

/// Legacy terminal encoding (standard escape sequences).
fn encode_legacy(key: TerminalKey) -> Vec<u8> {
    let mods = key.modifiers;

    // Modified special keys (arrows, home, end, etc.) use xterm format:
    //   \x1b[1;{modifier}A  for arrows/home/end
    //   \x1b[{n};{modifier}~ for insert/delete/pgup/pgdn
    // The ESC-prefix hack doesn't work for these since they're already escape sequences.
    if !mods.is_empty()
        && let Some(bytes) = encode_modified_special(key.code, mods)
    {
        return bytes;
    }

    // Alt modifier on character keys: prefix with ESC
    if mods.contains(KeyModifiers::ALT) {
        let inner = key.with_modifiers(mods.difference(KeyModifiers::ALT));
        let mut bytes = vec![0x1b];
        bytes.extend(encode_legacy_inner(&inner));
        return bytes;
    }
    encode_legacy_inner(&key)
}

/// xterm-style encoding for modified special keys.
/// Modifier value: 1 + (shift?1:0) + (alt?2:0) + (ctrl?4:0)
fn encode_modified_special(code: KeyCode, mods: KeyModifiers) -> Option<Vec<u8>> {
    let modifier = xterm_modifier(mods);
    if modifier <= 1 {
        return None; // no modifiers to encode
    }

    let key = functional_key(code)?;
    Some(format!("\x1b[{};{modifier}{}", key.number, key.final_byte).into_bytes())
}

fn xterm_modifier(mods: KeyModifiers) -> u32 {
    1 + modifier_bits(mods, false)
}

fn kitty_modifier(mods: KeyModifiers) -> u32 {
    1 + modifier_bits(mods, true)
}

fn encode_text_input(key: &TerminalKey) -> Option<Vec<u8>> {
    let ch = text_char_for_key(key)?;
    let mut buf = [0u8; UTF8_MAX_BYTES_PER_CODEPOINT];
    Some(ch.encode_utf8(&mut buf).as_bytes().to_vec())
}

fn kitty_legacy_text_key(key: &TerminalKey) -> bool {
    let KeyCode::Char(ch) = key.code else {
        return false;
    };
    if !ch.is_ascii_graphic() && ch != ' ' {
        return false;
    }

    let mods = key.modifiers;
    mods == KeyModifiers::ALT
        || mods == KeyModifiers::CONTROL
        || mods == (KeyModifiers::ALT | KeyModifiers::SHIFT)
        || mods == (KeyModifiers::CONTROL | KeyModifiers::ALT)
        // Kitty's legacy table includes Ctrl+Shift+Space as NUL. Other
        // Ctrl+Shift character chords retain Shift through CSI u.
        || (ch == ' ' && mods == (KeyModifiers::CONTROL | KeyModifiers::SHIFT))
}

fn text_char_for_key(key: &TerminalKey) -> Option<char> {
    if key.kind == crossterm::event::KeyEventKind::Release {
        return None;
    }

    let KeyCode::Char(ch) = key.code else {
        return None;
    };

    if key.modifiers.is_empty() {
        return Some(ch);
    }
    if key.modifiers == KeyModifiers::SHIFT {
        return shifted_text_char(key, ch);
    }
    None
}

fn shifted_text_char(key: &TerminalKey, ch: char) -> Option<char> {
    // Kitty text fallback accepts reported alternates, ASCII letters and
    // already shifted punctuation, but does not guess a punctuation layout.
    if key.shifted_codepoint.is_some()
        || ch.is_ascii_alphabetic()
        || super::is_shifted_ascii_symbol(ch)
    {
        key.produced_char()
    } else {
        None
    }
}

fn canonical_kitty_char(ch: char, mods: KeyModifiers) -> char {
    if mods.contains(KeyModifiers::SHIFT) && ch.is_ascii_uppercase() {
        ch.to_ascii_lowercase()
    } else {
        ch
    }
}

fn alternate_shifted_codepoint(key: &TerminalKey, flags: KittyKeyboardFlags) -> Option<char> {
    if !flags.contains(KittyKeyboardFlags::REPORT_ALTERNATE_KEYS) {
        return None;
    }

    if let Some(shifted) = key.shifted_codepoint {
        return Some(shifted);
    }

    match key.code {
        KeyCode::Char(ch)
            if key.modifiers.contains(KeyModifiers::SHIFT) && ch.is_ascii_uppercase() =>
        {
            Some(ch)
        }
        _ => None,
    }
}

fn kitty_event_suffix(key: &TerminalKey, flags: KittyKeyboardFlags) -> Option<u8> {
    if !flags.contains(KittyKeyboardFlags::REPORT_EVENT_TYPES) {
        return None;
    }

    Some(match key.kind {
        crossterm::event::KeyEventKind::Press => 1,
        crossterm::event::KeyEventKind::Repeat => 2,
        crossterm::event::KeyEventKind::Release => 3,
    })
}

fn encode_legacy_inner(key: &TerminalKey) -> Vec<u8> {
    match key.code {
        KeyCode::Char(ch) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                control_byte(ch).map_or_else(|| ch.to_string().into_bytes(), |byte| vec![byte])
            } else {
                let ch = if key.modifiers == KeyModifiers::SHIFT {
                    key.produced_char().unwrap_or(ch)
                } else {
                    ch
                };
                let mut buf = [0u8; UTF8_MAX_BYTES_PER_CODEPOINT];
                ch.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![127],
        KeyCode::Tab => vec![9],
        KeyCode::BackTab => vec![27, 91, 90],
        KeyCode::Esc => vec![27],
        code => functional_key(code).map_or_else(Vec::new, |key| key.legacy.as_bytes().to_vec()),
    }
}

#[cfg(test)]
use crossterm::event::KeyEvent;

/// Encode a key event for a PTY child using the supported subset of the pane's
/// negotiated keyboard protocol.
/// Full Kitty report-all fidelity is deliberately omitted because agent and shell
/// panes do not need a shepr-owned key model.
/// Test-only: production keys go through `encode_terminal_key_with_modes`.
#[cfg(test)]
fn encode_key(key: KeyEvent, protocol: KeyboardProtocol) -> Vec<u8> {
    encode_terminal_key(key.into(), protocol)
}

/// Test-only: production applies DECCKM in `encode_terminal_key_with_modes`.
#[cfg(test)]
fn encode_cursor_key(code: KeyCode, application_cursor: bool) -> Vec<u8> {
    encode_terminal_key_with_modes(
        TerminalKey::new(code, KeyModifiers::empty()),
        KeyEncodeModes {
            application_cursor,
            ..KeyEncodeModes::default()
        },
    )
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    fn kitty_protocol(flags: u16) -> KeyboardProtocol {
        KeyboardProtocol::from_flags(KittyKeyboardFlags::from_bits_retain(flags))
    }

    #[test]
    fn generated_text_is_sent_as_text() {
        let key = TerminalKey::new(KeyCode::Char('/'), KeyModifiers::SHIFT)
            .with_generated_text(Some("/".to_owned()));
        for protocol in [KeyboardProtocol::legacy(), kitty_protocol(1)] {
            assert_eq!(
                encode_terminal_key(key.clone(), protocol),
                b"/",
                "{protocol:?}"
            );
        }
    }

    #[test]
    fn report_all_keys_encodes_committed_repeats_with_event_type() {
        let key = TerminalKey::new(KeyCode::Char('j'), KeyModifiers::empty())
            .with_text_commit()
            .with_kind(crossterm::event::KeyEventKind::Repeat);
        assert_eq!(key.generated_text.as_deref(), Some("j"));
        assert_eq!(
            encode_terminal_key(key, kitty_protocol(27)),
            b"\x1b[106;1:2;106u"
        );
    }

    #[test]
    fn associated_text_carries_every_committed_codepoint() {
        let key = TerminalKey::new(KeyCode::Char('e'), KeyModifiers::empty())
            .with_generated_text(Some("e\u{301}".to_owned()));
        assert_eq!(
            encode_terminal_key(key, kitty_protocol(24)),
            b"\x1b[101;1;101:769u"
        );

        // Control characters are forbidden in the field and are left out;
        // text made only of them leaves no field at all.
        let control_only = TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty())
            .with_generated_text(Some("\u{7}".to_owned()));
        assert_eq!(
            encode_terminal_key(control_only, kitty_protocol(24)),
            b"\x1b[120;1u"
        );
    }

    #[test]
    fn report_all_keys_keeps_text_of_a_committed_key_with_no_csi_u_form() {
        let key = TerminalKey::new(KeyCode::Null, KeyModifiers::empty())
            .with_generated_text(Some("x".to_owned()));
        assert_eq!(encode_terminal_key(key, kitty_protocol(24)), b"x");
    }

    #[test]
    fn legacy_enter() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
        assert_eq!(encode_key(key, KeyboardProtocol::legacy()), vec![b'\r']);
    }

    #[test]
    fn legacy_ctrl_c() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(encode_key(key, KeyboardProtocol::legacy()), vec![3]);
    }

    #[test]
    fn legacy_ctrl_slash_aliases_ctrl_underscore() {
        let key = KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL);
        assert_eq!(encode_key(key, KeyboardProtocol::legacy()), vec![31]);
    }

    #[test]
    fn legacy_ctrl_question_and_eight_send_del() {
        for ch in ['?', '8'] {
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL);
            assert_eq!(encode_key(key, KeyboardProtocol::legacy()), vec![127]);
        }
    }

    #[test]
    fn legacy_ctrl_non_ascii_char_uses_utf8() {
        let key = KeyEvent::new(KeyCode::Char('ß'), KeyModifiers::CONTROL);
        assert_eq!(encode_key(key, KeyboardProtocol::legacy()), "ß".as_bytes());
    }

    #[test]
    fn legacy_shift_enter_is_just_cr() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, KeyboardProtocol::legacy()), vec![b'\r']);
    }

    #[test]
    fn legacy_alt_up() {
        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(encode_key(key, KeyboardProtocol::legacy()), b"\x1b[1;3A");
    }

    #[test]
    fn legacy_shift_right() {
        let key = KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, KeyboardProtocol::legacy()), b"\x1b[1;2C");
    }

    #[test]
    fn legacy_ctrl_left() {
        let key = KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL);
        assert_eq!(encode_key(key, KeyboardProtocol::legacy()), b"\x1b[1;5D");
    }

    #[test]
    fn legacy_ctrl_shift_end() {
        let key = KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, KeyboardProtocol::legacy()), b"\x1b[1;6F");
    }

    #[test]
    fn legacy_alt_delete() {
        let key = KeyEvent::new(KeyCode::Delete, KeyModifiers::ALT);
        assert_eq!(encode_key(key, KeyboardProtocol::legacy()), b"\x1b[3;3~");
    }

    #[test]
    fn legacy_shift_f5() {
        let key = KeyEvent::new(KeyCode::F(5), KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, KeyboardProtocol::legacy()), b"\x1b[15;2~");
    }

    #[test]
    fn legacy_alt_char_still_esc_prefix() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT);
        assert_eq!(encode_key(key, KeyboardProtocol::legacy()), b"\x1ba");
    }

    #[test]
    fn legacy_alt_backspace_sends_escape_delete() {
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(encode_key(key, KeyboardProtocol::legacy()), b"\x1b\x7f");
    }

    #[test]
    fn application_cursor_keys_use_ss3_sequences() {
        assert_eq!(encode_cursor_key(KeyCode::Up, true), b"\x1bOA");
        assert_eq!(encode_cursor_key(KeyCode::Down, true), b"\x1bOB");
    }

    #[test]
    fn normal_cursor_keys_use_csi_sequences() {
        assert_eq!(encode_cursor_key(KeyCode::Up, false), b"\x1b[A");
        assert_eq!(encode_cursor_key(KeyCode::Down, false), b"\x1b[B");
    }

    #[test]
    fn kitty_shift_enter() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, kitty_protocol(1)), b"\x1b[13;2u");
    }

    #[test]
    fn kitty_ctrl_shift_a() {
        let key = KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert_eq!(encode_key(key, kitty_protocol(1)), b"\x1b[97;6u");
    }

    #[test]
    fn kitty_shift_uppercase_letter_sends_text() {
        let key = KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, kitty_protocol(1)), b"L");
    }

    #[test]
    fn kitty_shift_uppercase_letter_ignores_alternate_key_reporting_for_text() {
        let key = KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, kitty_protocol(7)), b"L");
    }

    #[test]
    fn kitty_shift_lowercase_letter_sends_uppercase_text() {
        let key = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, kitty_protocol(1)), b"L");
    }

    #[test]
    fn kitty_alt_shift_uppercase_letter_uses_base_codepoint() {
        let key = KeyEvent::new(KeyCode::Char('L'), KeyModifiers::ALT | KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, kitty_protocol(1)), b"\x1b[108;4u");
    }

    #[test]
    fn kitty_ctrl_shift_uppercase_letter_uses_base_codepoint() {
        let key = KeyEvent::new(
            KeyCode::Char('L'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert_eq!(encode_key(key, kitty_protocol(1)), b"\x1b[108;6u");
    }

    #[test]
    fn legacy_shift_uppercase_letter_stays_uppercase() {
        let key = KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, KeyboardProtocol::legacy()), b"L");
    }

    #[test]
    fn kitty_alt_enter() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        assert_eq!(encode_key(key, kitty_protocol(1)), b"\x1b[13;3u");
    }

    #[test]
    fn kitty_alt_backspace_uses_csi_u() {
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(encode_key(key, kitty_protocol(1)), b"\x1b[127;3u");
    }

    #[test]
    fn kitty_plain_ctrl_c_uses_csi_u() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(encode_key(key, kitty_protocol(1)), b"\x1b[99;5u");
    }

    #[test]
    fn kitty_non_disambiguating_enhancements_keep_legacy_text_chords() {
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        for flags in [2, 4] {
            assert_eq!(
                encode_key(ctrl_a, kitty_protocol(flags)),
                b"\x01",
                "flags={flags}"
            );
        }

        assert_eq!(encode_key(ctrl_a, kitty_protocol(1)), b"\x1b[97;5u");

        let ctrl_a_repeat = KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
            crossterm::event::KeyEventKind::Repeat,
        );
        assert_eq!(
            encode_key(ctrl_a_repeat, kitty_protocol(2)),
            b"\x1b[97;5:2u"
        );
    }

    #[test]
    fn kitty_plain_ctrl_c_includes_press_event_when_requested() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(encode_key(key, kitty_protocol(3)), b"\x1b[99;5:1u");
    }

    #[test]
    fn kitty_unmodified_uses_legacy() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty());
        assert_eq!(encode_key(key, kitty_protocol(1)), b"a");
    }

    #[test]
    fn kitty_report_event_types_keeps_basic_compatibility_keys_legacy() {
        let cases = [
            (KeyCode::Enter, b"\r".as_slice()),
            (KeyCode::Tab, b"\t".as_slice()),
            (KeyCode::Backspace, b"\x7f".as_slice()),
        ];

        for (code, expected) in cases {
            let press = KeyEvent::new_with_kind(
                code,
                KeyModifiers::empty(),
                crossterm::event::KeyEventKind::Press,
            );
            assert_eq!(
                encode_key(press, kitty_protocol(3)),
                expected,
                "{code:?} press should stay legacy-compatible without REPORT_ALL_KEYS"
            );

            let repeat = KeyEvent::new_with_kind(
                code,
                KeyModifiers::empty(),
                crossterm::event::KeyEventKind::Repeat,
            );
            assert_eq!(
                encode_key(repeat, kitty_protocol(3)),
                expected,
                "{code:?} repeat should stay legacy-compatible without REPORT_ALL_KEYS"
            );

            let release = KeyEvent::new_with_kind(
                code,
                KeyModifiers::empty(),
                crossterm::event::KeyEventKind::Release,
            );
            assert_eq!(
                encode_key(release, kitty_protocol(3)),
                b"",
                "{code:?} release should not fall back to legacy bytes"
            );
        }
    }

    #[test]
    fn kitty_report_all_keys_encodes_basic_compatibility_keys_with_events() {
        let enter_press = KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::empty(),
            crossterm::event::KeyEventKind::Press,
        );
        assert_eq!(encode_key(enter_press, kitty_protocol(9)), b"\x1b[13;1u");

        let backspace_press = KeyEvent::new_with_kind(
            KeyCode::Backspace,
            KeyModifiers::empty(),
            crossterm::event::KeyEventKind::Press,
        );
        assert_eq!(
            encode_key(backspace_press, kitty_protocol(11)),
            b"\x1b[127;1:1u"
        );

        let backspace_release = KeyEvent::new_with_kind(
            KeyCode::Backspace,
            KeyModifiers::empty(),
            crossterm::event::KeyEventKind::Release,
        );
        assert_eq!(
            encode_key(backspace_release, kitty_protocol(11)),
            b"\x1b[127;1:3u"
        );
    }

    #[test]
    fn kitty_report_all_keys_encodes_printable_event_kinds() {
        for (kind, expected) in [
            (
                crossterm::event::KeyEventKind::Press,
                b"\x1b[106;1:1u".as_slice(),
            ),
            (
                crossterm::event::KeyEventKind::Repeat,
                b"\x1b[106;1:2u".as_slice(),
            ),
            (
                crossterm::event::KeyEventKind::Release,
                b"\x1b[106;1:3u".as_slice(),
            ),
        ] {
            let key = KeyEvent::new_with_kind(KeyCode::Char('j'), KeyModifiers::empty(), kind);
            assert_eq!(encode_key(key, kitty_protocol(15)), expected);
        }
    }

    #[test]
    fn kitty_report_associated_text_embeds_shifted_printables() {
        let cases = [
            (
                TerminalKey::new(KeyCode::Char('A'), KeyModifiers::SHIFT),
                b"\x1b[97;2;65u".as_slice(),
            ),
            (
                TerminalKey::new(KeyCode::Char('1'), KeyModifiers::SHIFT)
                    .with_shifted_codepoint('!'),
                b"\x1b[49;2;33u".as_slice(),
            ),
            (
                TerminalKey::new(KeyCode::Char(':'), KeyModifiers::SHIFT),
                b"\x1b[58;2;58u".as_slice(),
            ),
        ];

        for (key, expected) in cases {
            assert_eq!(encode_terminal_key(key, kitty_protocol(25)), expected);
        }
    }

    #[test]
    fn kitty_associated_text_composes_with_alternates_and_events() {
        for (kind, expected) in [
            (
                crossterm::event::KeyEventKind::Press,
                b"\x1b[97:65;2:1;65u".as_slice(),
            ),
            (
                crossterm::event::KeyEventKind::Repeat,
                b"\x1b[97:65;2:2;65u".as_slice(),
            ),
            (
                crossterm::event::KeyEventKind::Release,
                b"\x1b[97:65;2:3u".as_slice(),
            ),
        ] {
            let key = TerminalKey::new(KeyCode::Char('A'), KeyModifiers::SHIFT).with_kind(kind);
            assert_eq!(encode_terminal_key(key, kitty_protocol(31)), expected);
        }
    }

    #[test]
    fn kitty_printable_release_is_encoded_without_report_all() {
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('j'),
            KeyModifiers::empty(),
            crossterm::event::KeyEventKind::Release,
        );
        assert_eq!(encode_key(release, kitty_protocol(3)), b"\x1b[106;1:3u");

        let mut malformed_release = TerminalKey::from(release);
        malformed_release.generated_text = Some("j".to_owned());
        assert_eq!(
            encode_terminal_key(malformed_release, kitty_protocol(3)),
            b"\x1b[106;1:3u"
        );
    }

    #[test]
    fn kitty_shift_tab() {
        let key = KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, kitty_protocol(1)), b"\x1b[9;2u");
    }

    #[test]
    fn kitty_ctrl_shift_enter() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, kitty_protocol(1)), b"\x1b[13;6u");
    }

    #[test]
    fn kitty_repeat_event_type_is_encoded_when_requested() {
        let key = KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::SHIFT,
            crossterm::event::KeyEventKind::Repeat,
        );
        assert_eq!(encode_key(key, kitty_protocol(3)), b"\x1b[13;2:2u");
    }

    #[test]
    fn kitty_shift_letter_release_uses_csi_u() {
        let key = KeyEvent::new_with_kind(
            KeyCode::Char('L'),
            KeyModifiers::SHIFT,
            crossterm::event::KeyEventKind::Release,
        );
        assert_eq!(encode_key(key, kitty_protocol(7)), b"\x1b[108:76;2:3u");
    }

    #[test]
    fn kitty_shifted_punctuation_literals_send_text() {
        for ch in "!@#$%^&*()_+{}|:\"<>?~".chars() {
            let key = TerminalKey::new(KeyCode::Char(ch), KeyModifiers::SHIFT);
            let encoded = encode_terminal_key(key, kitty_protocol(7));
            assert_eq!(encoded, ch.to_string().into_bytes(), "ch={ch}");
        }
    }

    #[test]
    fn kitty_shifted_punctuation_release_does_not_emit_text() {
        let key = TerminalKey::new(KeyCode::Char('?'), KeyModifiers::SHIFT)
            .with_kind(crossterm::event::KeyEventKind::Release);
        assert_eq!(encode_terminal_key(key, kitty_protocol(7)), b"\x1b[63;2:3u");
    }

    #[test]
    fn kitty_shifted_punctuation_does_not_infer_layout() {
        let key = TerminalKey::new(KeyCode::Char('1'), KeyModifiers::SHIFT);
        assert_eq!(encode_terminal_key(key, kitty_protocol(7)), b"\x1b[49;2:1u");
    }

    #[test]
    fn kitty_modified_shifted_punctuation_stays_modified_key() {
        for (modifiers, expected) in [
            (
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                b"\x1b[33;6:1u".as_slice(),
            ),
            (
                KeyModifiers::ALT | KeyModifiers::SHIFT,
                b"\x1b[33;4:1u".as_slice(),
            ),
            (
                KeyModifiers::SUPER | KeyModifiers::SHIFT,
                b"\x1b[33;10:1u".as_slice(),
            ),
        ] {
            let key = TerminalKey::new(KeyCode::Char('!'), modifiers);
            let encoded = encode_terminal_key(key, kitty_protocol(7));
            assert_eq!(encoded, expected, "modifiers={modifiers:?}");
        }
    }

    #[test]
    fn release_bytes_gated_on_report_event_types() {
        for code in [KeyCode::Enter, KeyCode::Backspace] {
            let release = KeyEvent::new_with_kind(
                code,
                KeyModifiers::empty(),
                crossterm::event::KeyEventKind::Release,
            );

            // Legacy and Kitty disambiguate-only (no REPORT_EVENT_TYPES) must not
            // emit a byte on release, otherwise Enter/Backspace double.
            assert_eq!(encode_key(release, KeyboardProtocol::legacy()), b"");
            assert_eq!(encode_key(release, kitty_protocol(1)), b"");
        }

        let modified_release = KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
            crossterm::event::KeyEventKind::Release,
        );
        assert_eq!(
            encode_key(modified_release, kitty_protocol(3)),
            b"\x1b[13;5:3u"
        );
    }

    #[test]
    fn kitty_shifted_symbol_sends_text() {
        let key =
            TerminalKey::new(KeyCode::Char('1'), KeyModifiers::SHIFT).with_shifted_codepoint('!');
        assert_eq!(encode_terminal_key(key, kitty_protocol(7)), b"!");
    }

    #[test]
    fn kitty_shifted_symbol_prefers_text_over_roundtrip_key_identity() {
        let key =
            TerminalKey::new(KeyCode::Char('1'), KeyModifiers::SHIFT).with_shifted_codepoint('!');
        let encoded = encode_terminal_key(key, kitty_protocol(7));
        assert_eq!(encoded, b"!");
    }

    #[test]
    fn kitty_shifted_symbol_pair_matrix_is_encoded_as_text() {
        let cases = [('1', '!'), ('/', '?'), ('[', '{')];

        for (base, shifted) in cases {
            let key = TerminalKey::new(KeyCode::Char(base), KeyModifiers::SHIFT)
                .with_shifted_codepoint(shifted);
            let encoded = encode_terminal_key(key, kitty_protocol(7));
            assert_eq!(encoded, shifted.to_string().into_bytes(), "base={base}");
        }
    }

    #[test]
    fn chinese_char_encodes_as_utf8() {
        let key = TerminalKey::new(KeyCode::Char('中'), KeyModifiers::empty());
        let encoded = encode_terminal_key(key, KeyboardProtocol::legacy());
        assert_eq!(encoded, "中".as_bytes());
    }

    #[test]
    fn chinese_char_with_kitty_protocol_encodes_as_utf8() {
        let key = TerminalKey::new(KeyCode::Char('文'), KeyModifiers::empty());
        let encoded = encode_terminal_key(key, kitty_protocol(7));
        assert_eq!(encoded, "文".as_bytes());
    }

    #[test]
    fn mode_aware_encoder_honours_application_cursor_keys() {
        let modes = KeyEncodeModes {
            application_cursor: true,
            ..KeyEncodeModes::default()
        };
        for (code, expected) in [
            (KeyCode::Up, b"\x1bOA".as_slice()),
            (KeyCode::Left, b"\x1bOD".as_slice()),
            (KeyCode::Home, b"\x1bOH".as_slice()),
            (KeyCode::End, b"\x1bOF".as_slice()),
            (KeyCode::PageUp, b"\x1b[5~".as_slice()),
        ] {
            let key = TerminalKey::new(code, KeyModifiers::empty());
            assert_eq!(
                encode_terminal_key_with_modes(key, modes),
                expected,
                "{code:?}"
            );
        }
        // Modified cursor keys keep the CSI 1;m form.
        let key = TerminalKey::new(KeyCode::Up, KeyModifiers::CONTROL);
        assert_eq!(encode_terminal_key_with_modes(key, modes), b"\x1b[1;5A");
    }

    #[test]
    fn mode_aware_encoder_speaks_modify_other_keys() {
        let level = |modify_other_keys| KeyEncodeModes {
            modify_other_keys,
            ..KeyEncodeModes::default()
        };
        let enter = |modifiers| TerminalKey::new(KeyCode::Enter, modifiers);

        assert_eq!(
            encode_terminal_key_with_modes(
                enter(KeyModifiers::SHIFT),
                level(ModifyOtherKeysLevel::ExceptWellDefined)
            ),
            b"\x1b[27;2;13~"
        );
        assert_eq!(
            encode_terminal_key_with_modes(
                enter(KeyModifiers::SUPER),
                level(ModifyOtherKeysLevel::ExceptWellDefined)
            ),
            b"\x1b[27;9;13~"
        );
        assert_eq!(
            encode_terminal_key_with_modes(
                enter(KeyModifiers::ALT),
                level(ModifyOtherKeysLevel::ExceptWellDefined)
            ),
            b"\x1b\r"
        );
        assert_eq!(
            encode_terminal_key_with_modes(
                enter(KeyModifiers::ALT),
                level(ModifyOtherKeysLevel::All)
            ),
            b"\x1b[27;3;13~"
        );
        assert_eq!(
            encode_terminal_key_with_modes(
                enter(KeyModifiers::SHIFT),
                level(ModifyOtherKeysLevel::Off)
            ),
            b"\r"
        );
        assert_eq!(
            encode_terminal_key_with_modes(
                TerminalKey::new(KeyCode::Tab, KeyModifiers::SHIFT),
                level(ModifyOtherKeysLevel::ExceptWellDefined)
            ),
            b"\x1b[Z"
        );
        assert_eq!(
            encode_terminal_key_with_modes(
                TerminalKey::new(KeyCode::Tab, KeyModifiers::SHIFT),
                level(ModifyOtherKeysLevel::All)
            ),
            b"\x1b[27;2;9~"
        );
        assert_eq!(
            encode_terminal_key_with_modes(
                TerminalKey::new(KeyCode::Backspace, KeyModifiers::CONTROL),
                level(ModifyOtherKeysLevel::Off)
            ),
            b"\x08"
        );
    }

    #[test]
    fn mode_aware_encoder_maps_backtab_and_escape_under_kitty() {
        assert_eq!(
            encode_terminal_key_with_modes(
                TerminalKey::new(KeyCode::Tab, KeyModifiers::SHIFT),
                KeyEncodeModes::default(),
            ),
            b"\x1b[Z"
        );

        let kitty = KeyEncodeModes {
            kitty_flags: KittyKeyboardFlags::DISAMBIGUATE,
            ..KeyEncodeModes::default()
        };
        assert_eq!(
            encode_terminal_key_with_modes(
                TerminalKey::new(KeyCode::BackTab, KeyModifiers::SHIFT),
                kitty
            ),
            b"\x1b[9;2u"
        );
        assert_eq!(
            encode_terminal_key_with_modes(
                TerminalKey::new(KeyCode::Esc, KeyModifiers::empty()),
                kitty
            ),
            b"\x1b[27u"
        );
        assert_eq!(
            encode_terminal_key_with_modes(
                TerminalKey::new(KeyCode::BackTab, KeyModifiers::SHIFT),
                KeyEncodeModes::default()
            ),
            b"\x1b[Z"
        );
    }

    #[test]
    fn chinese_char_with_modifiers_falls_back_to_kitty_encoding() {
        let key = TerminalKey::new(KeyCode::Char('测'), KeyModifiers::ALT);
        let encoded = encode_terminal_key(key, kitty_protocol(7));
        assert!(!encoded.is_empty());
        assert_ne!(encoded, "测".as_bytes());
    }

    #[test]
    fn protocol_from_zero_flags_is_legacy() {
        let protocol = KeyboardProtocol::from_flags(KittyKeyboardFlags::NONE);
        assert_eq!(protocol, KeyboardProtocol::legacy());
        assert!(!protocol.is_kitty());
    }

    #[test]
    fn protocol_from_nonzero_flags_is_kitty() {
        let flags = KittyKeyboardFlags::from_bits_retain(7);
        let protocol = KeyboardProtocol::from_flags(flags);
        assert!(protocol.is_kitty());
        assert_eq!(protocol.kitty_flags(), flags);
    }
}
