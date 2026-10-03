use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use std::fmt::Write as _;

use super::tables::{control_byte, functional_key, modifier_bits};
use super::{KeyboardProtocol, MouseProtocolEncoding, MouseProtocolMode, TerminalKey};
use crate::limits::{KITTY_KEY_SEQUENCE_INITIAL_CAPACITY, UTF8_MOUSE_REPORT_INITIAL_CAPACITY};
use shepr_config::BindingKey;
use shepr_core::limits::UTF8_MAX_BYTES_PER_CODEPOINT;
use shepr_protocol::KittyKeyboardFlags;
use shepr_vt::ModifyOtherKeysLevel;

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

/// `column` and `row` are the final 1-based coordinates to report.
fn encode_mouse_cb(
    base_button: u16,
    release: bool,
    column: u32,
    row: u32,
    modifiers: KeyModifiers,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    // Mouse reports are not a bijection: legacy release loses the button,
    // and host decoding also accepts extended-button motion we cannot emit.
    // SGR reports which button was released; the legacy encodings report
    // every release as button 3.
    let sgr = matches!(
        encoding,
        MouseProtocolEncoding::Sgr | MouseProtocolEncoding::SgrPixels
    );
    let mut cb = if release && !sgr { 3 } else { base_button };
    if modifiers.contains(KeyModifiers::SHIFT) {
        cb += 4;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        cb += 8;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        cb += 16;
    }

    match encoding {
        MouseProtocolEncoding::Sgr | MouseProtocolEncoding::SgrPixels => Some(
            format!(
                "\x1b[<{cb};{column};{row}{}",
                if release { 'm' } else { 'M' }
            )
            .into_bytes(),
        ),
        MouseProtocolEncoding::Default => {
            let cb = u8::try_from(cb + 32).ok()?;
            let column = u8::try_from(column + 32).ok()?;
            let row = u8::try_from(row + 32).ok()?;
            Some(vec![0x1b, b'[', b'M', cb, column, row])
        }
        MouseProtocolEncoding::Utf8 => {
            // UTF-8 mouse mode encodes coordinates as one or two UTF-8 bytes;
            // xterm's extended-coordinate range ends at position 2015.
            if column > 2015 || row > 2015 {
                return None;
            }
            let mut bytes = Vec::with_capacity(UTF8_MOUSE_REPORT_INITIAL_CAPACITY);
            bytes.extend_from_slice(b"\x1b[M");
            push_mouse_codepoint(&mut bytes, cb as u32 + 32)?;
            push_mouse_codepoint(&mut bytes, column + 32)?;
            push_mouse_codepoint(&mut bytes, row + 32)?;
            Some(bytes)
        }
    }
}

fn push_mouse_codepoint(bytes: &mut Vec<u8>, value: u32) -> Option<()> {
    let ch = char::from_u32(value)?;
    let mut buf = [0u8; UTF8_MAX_BYTES_PER_CODEPOINT];
    bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    Some(())
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
        write!(&mut sequence, ":{shifted}").ok()?;
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
        let (canonical_code, canonical_modifiers) = key.canonical_key();
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

/// Encode a mouse event as an xterm mouse report. `x` and `y` are 1-based
/// cell coordinates (or pixel coordinates for SGR-pixels). Returns `None`
/// when the protocol mode does not report this kind of event or the position
/// cannot be represented in the encoding.
pub fn encode_mouse_event(
    kind: MouseEventKind,
    x: u32,
    y: u32,
    modifiers: KeyModifiers,
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    let button_code = |button: MouseButton| -> u16 {
        match button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
        }
    };
    let (base_button, release) = match kind {
        MouseEventKind::Down(button) => (button_code(button), false),
        MouseEventKind::Up(button) => (button_code(button), true),
        MouseEventKind::Drag(button) => (button_code(button) + 32, false),
        MouseEventKind::Moved => (3 + 32, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
    };
    let reported = match mode {
        // X10 reports button presses only.
        MouseProtocolMode::Press => (!release && base_button < 32) || base_button >= 64,
        MouseProtocolMode::PressRelease => !(32..64).contains(&base_button),
        MouseProtocolMode::ButtonMotion => kind != MouseEventKind::Moved,
        MouseProtocolMode::AnyMotion => true,
    };
    if !reported {
        return None;
    }
    let modifiers = if mode == MouseProtocolMode::Press {
        KeyModifiers::empty()
    } else {
        modifiers
    };
    encode_mouse_cb(base_button, release, x, y, modifiers, encoding)
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
    if let Some(shifted) = key.shifted_codepoint.and_then(char::from_u32) {
        return Some(shifted);
    }

    if ch.is_ascii_uppercase() {
        return Some(ch);
    }

    if ch.is_ascii_lowercase() {
        return Some(ch.to_ascii_uppercase());
    }

    if is_shifted_ascii_punctuation(ch) {
        return Some(ch);
    }

    None
}

/// Shift applied to an unshifted US-layout key. Only the legacy encoding
/// guesses this, with copy mode's table; the kitty protocol reports the base
/// key instead of inferring a layout.
fn shifted_ascii_punctuation(ch: char) -> Option<char> {
    crate::copy_mode::shifted_ascii_char(ch)
}

fn is_shifted_ascii_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '!' | '@'
            | '#'
            | '$'
            | '%'
            | '^'
            | '&'
            | '*'
            | '('
            | ')'
            | '_'
            | '+'
            | '{'
            | '}'
            | '|'
            | ':'
            | '"'
            | '<'
            | '>'
            | '?'
            | '~'
    )
}

fn canonical_kitty_char(ch: char, mods: KeyModifiers) -> char {
    if mods.contains(KeyModifiers::SHIFT) && ch.is_ascii_uppercase() {
        ch.to_ascii_lowercase()
    } else {
        ch
    }
}

fn alternate_shifted_codepoint(key: &TerminalKey, flags: KittyKeyboardFlags) -> Option<u32> {
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
            Some(ch as u32)
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
                    shifted_text_char(key, ch)
                        .or_else(|| shifted_ascii_punctuation(ch))
                        .unwrap_or(ch)
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

/// Test-only: production mouse reports go through `encode_mouse_event`.
#[cfg(test)]
fn encode_mouse_scroll(
    kind: MouseEventKind,
    column: u16,
    row: u16,
    modifiers: KeyModifiers,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    let button = match kind {
        MouseEventKind::ScrollUp => 64u16,
        MouseEventKind::ScrollDown => 65u16,
        MouseEventKind::ScrollLeft => 66u16,
        MouseEventKind::ScrollRight => 67u16,
        _ => return None,
    };
    encode_mouse_cb(
        button,
        false,
        u32::from(column) + 1,
        u32::from(row) + 1,
        modifiers,
        encoding,
    )
}

/// Test-only: production mouse reports go through `encode_mouse_event`.
#[cfg(test)]
fn encode_mouse_button(
    kind: MouseEventKind,
    column: u16,
    row: u16,
    modifiers: KeyModifiers,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    let (button, release) = match kind {
        MouseEventKind::Down(MouseButton::Left) => (0u16, false),
        MouseEventKind::Down(MouseButton::Middle) => (1u16, false),
        MouseEventKind::Down(MouseButton::Right) => (2u16, false),
        MouseEventKind::Up(MouseButton::Left) => (0u16, true),
        MouseEventKind::Up(MouseButton::Middle) => (1u16, true),
        MouseEventKind::Up(MouseButton::Right) => (2u16, true),
        MouseEventKind::Drag(MouseButton::Left) => (32u16, false),
        MouseEventKind::Drag(MouseButton::Middle) => (33u16, false),
        MouseEventKind::Drag(MouseButton::Right) => (34u16, false),
        _ => return None,
    };
    encode_mouse_cb(
        button,
        release,
        u32::from(column) + 1,
        u32::from(row) + 1,
        modifiers,
        encoding,
    )
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::input::parse_terminal_key_sequence;

    fn assert_terminal_key_eq(
        actual: &TerminalKey,
        code: KeyCode,
        modifiers: KeyModifiers,
        kind: crossterm::event::KeyEventKind,
        shifted_codepoint: Option<u32>,
    ) {
        assert_eq!(actual.code, code);
        assert_eq!(actual.modifiers, modifiers);
        assert_eq!(actual.kind, kind);
        assert_eq!(actual.shifted_codepoint, shifted_codepoint);
    }

    #[test]
    fn generated_text_is_sent_as_text() {
        let key = TerminalKey::new(KeyCode::Char('/'), KeyModifiers::SHIFT)
            .with_generated_text(Some("/".to_owned()));
        for protocol in [
            KeyboardProtocol::legacy(),
            KeyboardProtocol::from_kitty_flags(1),
        ] {
            assert_eq!(
                encode_terminal_key(key.clone(), protocol),
                b"/",
                "{protocol:?}"
            );
        }
    }

    #[test]
    fn report_all_keys_encodes_committed_text_as_csi_u() {
        let committed = |text: &str| {
            parse_terminal_key_sequence(text)
                .expect("test precondition")
                .with_text_commit()
        };
        let lower = committed("a");
        let upper = committed("A");
        assert_eq!(lower.generated_text.as_deref(), Some("a"));
        assert_eq!(upper.generated_text.as_deref(), Some("A"));

        for (key, flags, expected) in [
            (&lower, 8, b"\x1b[97;1u".as_slice()),
            (&lower, 9, b"\x1b[97;1u".as_slice()),
            (&lower, 24, b"\x1b[97;1;97u".as_slice()),
            (&lower, 11, b"\x1b[97;1:1u".as_slice()),
            (&upper, 24, b"\x1b[97;2;65u".as_slice()),
            (&upper, 28, b"\x1b[97:65;2;65u".as_slice()),
            (&upper, 31, b"\x1b[97:65;2:1;65u".as_slice()),
        ] {
            assert_eq!(
                encode_terminal_key(key.clone(), KeyboardProtocol::from_kitty_flags(flags)),
                expected,
                "flags={flags} key={key:?}"
            );
        }

        // Without REPORT_ALL_KEYS committed text stays plain text.
        for flags in [1, 3, 7, 17, 23] {
            assert_eq!(
                encode_terminal_key(upper.clone(), KeyboardProtocol::from_kitty_flags(flags)),
                b"A",
                "flags={flags}"
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
            encode_terminal_key(key, KeyboardProtocol::from_kitty_flags(27)),
            b"\x1b[106;1:2;106u"
        );
    }

    #[test]
    fn associated_text_carries_every_committed_codepoint() {
        let key = TerminalKey::new(KeyCode::Char('e'), KeyModifiers::empty())
            .with_generated_text(Some("e\u{301}".to_owned()));
        assert_eq!(
            encode_terminal_key(key, KeyboardProtocol::from_kitty_flags(24)),
            b"\x1b[101;1;101:769u"
        );

        // Control characters are forbidden in the field and are left out;
        // text made only of them leaves no field at all.
        let control_only = TerminalKey::new(KeyCode::Char('x'), KeyModifiers::empty())
            .with_generated_text(Some("\u{7}".to_owned()));
        assert_eq!(
            encode_terminal_key(control_only, KeyboardProtocol::from_kitty_flags(24)),
            b"\x1b[120;1u"
        );
    }

    #[test]
    fn report_all_keys_keeps_text_of_a_committed_key_with_no_csi_u_form() {
        let key = TerminalKey::new(KeyCode::Null, KeyModifiers::empty())
            .with_generated_text(Some("x".to_owned()));
        assert_eq!(
            encode_terminal_key(key, KeyboardProtocol::from_kitty_flags(24)),
            b"x"
        );
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
    fn legacy_shift_ascii_punctuation_matches_copy_mode_mapping() {
        for (base, shifted) in [
            ('1', '!'),
            ('2', '@'),
            ('3', '#'),
            ('4', '$'),
            ('5', '%'),
            ('6', '^'),
            ('7', '&'),
            ('8', '*'),
            ('9', '('),
            ('0', ')'),
            ('-', '_'),
            ('=', '+'),
            ('[', '{'),
            (']', '}'),
            ('\\', '|'),
            (';', ':'),
            ('\'', '"'),
            (',', '<'),
            ('.', '>'),
            ('/', '?'),
            ('`', '~'),
        ] {
            let key = TerminalKey::new(KeyCode::Char(base), KeyModifiers::SHIFT);
            assert_eq!(
                crate::copy_mode::copy_mode_command_char(&key),
                Some(shifted),
                "copy mode base={base}"
            );
            assert_eq!(
                encode_terminal_key(key, KeyboardProtocol::legacy()),
                shifted.to_string().as_bytes(),
                "base={base}"
            );
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
    fn legacy_alt_shift_punctuation_uses_shifted_text() {
        let key = parse_terminal_key_sequence("\x1b[44:60;4u").expect("test precondition");
        assert_eq!(
            encode_terminal_key(key, KeyboardProtocol::legacy()),
            b"\x1b<"
        );
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
    fn sgr_mouse_scroll_encodes_wheel_button_and_coordinates() {
        let encoded = encode_mouse_scroll(
            crossterm::event::MouseEventKind::ScrollDown,
            4,
            6,
            KeyModifiers::SHIFT,
            MouseProtocolEncoding::Sgr,
        )
        .expect("mouse scroll should encode");

        assert_eq!(encoded, b"\x1b[<69;5;7M");
    }

    #[test]
    fn sgr_mouse_release_keeps_button_code() {
        let encoded = encode_mouse_button(
            crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            11,
            9,
            KeyModifiers::empty(),
            MouseProtocolEncoding::Sgr,
        )
        .expect("mouse release should encode");

        assert_eq!(encoded, b"\x1b[<0;12;10m");
    }

    #[test]
    fn utf8_mouse_encoding_caps_coordinates_at_xterms_limit() {
        let encoded = encode_mouse_cb(
            0,
            false,
            2015,
            2015,
            KeyModifiers::empty(),
            MouseProtocolEncoding::Utf8,
        );
        assert_eq!(encoded, Some(b"\x1b[M \xdf\xbf\xdf\xbf".to_vec()));

        assert_eq!(
            encode_mouse_cb(
                0,
                false,
                2016,
                1,
                KeyModifiers::empty(),
                MouseProtocolEncoding::Utf8,
            ),
            None
        );
        assert_eq!(
            encode_mouse_cb(
                0,
                false,
                1,
                2016,
                KeyModifiers::empty(),
                MouseProtocolEncoding::Utf8,
            ),
            None
        );
    }

    #[test]
    fn kitty_shift_enter() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(
            encode_key(key, KeyboardProtocol::from_kitty_flags(1)),
            b"\x1b[13;2u"
        );
    }

    #[test]
    fn kitty_ctrl_shift_a() {
        let key = KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert_eq!(
            encode_key(key, KeyboardProtocol::from_kitty_flags(1)),
            b"\x1b[97;6u"
        );
    }

    #[test]
    fn kitty_shift_uppercase_letter_sends_text() {
        let key = KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, KeyboardProtocol::from_kitty_flags(1)), b"L");
    }

    #[test]
    fn kitty_shift_uppercase_letter_ignores_alternate_key_reporting_for_text() {
        let key = KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, KeyboardProtocol::from_kitty_flags(7)), b"L");
    }

    #[test]
    fn kitty_shift_lowercase_letter_sends_uppercase_text() {
        let key = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, KeyboardProtocol::from_kitty_flags(1)), b"L");
    }

    #[test]
    fn kitty_alt_shift_uppercase_letter_uses_base_codepoint() {
        let key = KeyEvent::new(KeyCode::Char('L'), KeyModifiers::ALT | KeyModifiers::SHIFT);
        assert_eq!(
            encode_key(key, KeyboardProtocol::from_kitty_flags(1)),
            b"\x1b[108;4u"
        );
    }

    #[test]
    fn kitty_ctrl_shift_uppercase_letter_uses_base_codepoint() {
        let key = KeyEvent::new(
            KeyCode::Char('L'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert_eq!(
            encode_key(key, KeyboardProtocol::from_kitty_flags(1)),
            b"\x1b[108;6u"
        );
    }

    #[test]
    fn legacy_shift_uppercase_letter_stays_uppercase() {
        let key = KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, KeyboardProtocol::legacy()), b"L");
    }

    #[test]
    fn kitty_alt_enter() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        assert_eq!(
            encode_key(key, KeyboardProtocol::from_kitty_flags(1)),
            b"\x1b[13;3u"
        );
    }

    #[test]
    fn kitty_alt_backspace_uses_csi_u() {
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(
            encode_key(key, KeyboardProtocol::from_kitty_flags(1)),
            b"\x1b[127;3u"
        );
    }

    #[test]
    fn kitty_plain_ctrl_c_uses_csi_u() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            encode_key(key, KeyboardProtocol::from_kitty_flags(1)),
            b"\x1b[99;5u"
        );
    }

    #[test]
    fn kitty_non_disambiguating_enhancements_keep_legacy_text_chords() {
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        for flags in [2, 4] {
            assert_eq!(
                encode_key(ctrl_a, KeyboardProtocol::from_kitty_flags(flags)),
                b"\x01",
                "flags={flags}"
            );
        }

        assert_eq!(
            encode_key(ctrl_a, KeyboardProtocol::from_kitty_flags(1)),
            b"\x1b[97;5u"
        );

        let ctrl_a_repeat = KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
            crossterm::event::KeyEventKind::Repeat,
        );
        assert_eq!(
            encode_key(ctrl_a_repeat, KeyboardProtocol::from_kitty_flags(2)),
            b"\x1b[97;5:2u"
        );
    }

    #[test]
    fn kitty_plain_ctrl_c_includes_press_event_when_requested() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            encode_key(key, KeyboardProtocol::from_kitty_flags(3)),
            b"\x1b[99;5:1u"
        );
    }

    #[test]
    fn kitty_unmodified_uses_legacy() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty());
        assert_eq!(encode_key(key, KeyboardProtocol::from_kitty_flags(1)), b"a");
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
                encode_key(press, KeyboardProtocol::from_kitty_flags(3)),
                expected,
                "{code:?} press should stay legacy-compatible without REPORT_ALL_KEYS"
            );

            let repeat = KeyEvent::new_with_kind(
                code,
                KeyModifiers::empty(),
                crossterm::event::KeyEventKind::Repeat,
            );
            assert_eq!(
                encode_key(repeat, KeyboardProtocol::from_kitty_flags(3)),
                expected,
                "{code:?} repeat should stay legacy-compatible without REPORT_ALL_KEYS"
            );

            let release = KeyEvent::new_with_kind(
                code,
                KeyModifiers::empty(),
                crossterm::event::KeyEventKind::Release,
            );
            assert_eq!(
                encode_key(release, KeyboardProtocol::from_kitty_flags(3)),
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
        assert_eq!(
            encode_key(enter_press, KeyboardProtocol::from_kitty_flags(9)),
            b"\x1b[13;1u"
        );

        let backspace_press = KeyEvent::new_with_kind(
            KeyCode::Backspace,
            KeyModifiers::empty(),
            crossterm::event::KeyEventKind::Press,
        );
        assert_eq!(
            encode_key(backspace_press, KeyboardProtocol::from_kitty_flags(11)),
            b"\x1b[127;1:1u"
        );

        let backspace_release = KeyEvent::new_with_kind(
            KeyCode::Backspace,
            KeyModifiers::empty(),
            crossterm::event::KeyEventKind::Release,
        );
        assert_eq!(
            encode_key(backspace_release, KeyboardProtocol::from_kitty_flags(11)),
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
            assert_eq!(
                encode_key(key, KeyboardProtocol::from_kitty_flags(15)),
                expected
            );
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
                    .with_shifted_codepoint('!' as u32),
                b"\x1b[49;2;33u".as_slice(),
            ),
            (
                TerminalKey::new(KeyCode::Char(':'), KeyModifiers::SHIFT),
                b"\x1b[58;2;58u".as_slice(),
            ),
        ];

        for (key, expected) in cases {
            assert_eq!(
                encode_terminal_key(key, KeyboardProtocol::from_kitty_flags(25)),
                expected
            );
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
            assert_eq!(
                encode_terminal_key(key, KeyboardProtocol::from_kitty_flags(31)),
                expected
            );
        }
    }

    #[test]
    fn kitty_printable_release_is_encoded_without_report_all() {
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('j'),
            KeyModifiers::empty(),
            crossterm::event::KeyEventKind::Release,
        );
        assert_eq!(
            encode_key(release, KeyboardProtocol::from_kitty_flags(3)),
            b"\x1b[106;1:3u"
        );

        let mut malformed_release = TerminalKey::from(release);
        malformed_release.generated_text = Some("j".to_owned());
        assert_eq!(
            encode_terminal_key(malformed_release, KeyboardProtocol::from_kitty_flags(3)),
            b"\x1b[106;1:3u"
        );
    }

    #[test]
    fn kitty_shift_tab() {
        let key = KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT);
        assert_eq!(
            encode_key(key, KeyboardProtocol::from_kitty_flags(1)),
            b"\x1b[9;2u"
        );
    }

    #[test]
    fn kitty_ctrl_shift_enter() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        assert_eq!(
            encode_key(key, KeyboardProtocol::from_kitty_flags(1)),
            b"\x1b[13;6u"
        );
    }

    #[test]
    fn kitty_repeat_event_type_is_encoded_when_requested() {
        let key = KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::SHIFT,
            crossterm::event::KeyEventKind::Repeat,
        );
        assert_eq!(
            encode_key(key, KeyboardProtocol::from_kitty_flags(3)),
            b"\x1b[13;2:2u"
        );
    }

    #[test]
    fn kitty_shift_letter_release_uses_csi_u() {
        let key = KeyEvent::new_with_kind(
            KeyCode::Char('L'),
            KeyModifiers::SHIFT,
            crossterm::event::KeyEventKind::Release,
        );
        assert_eq!(
            encode_key(key, KeyboardProtocol::from_kitty_flags(7)),
            b"\x1b[108:76;2:3u"
        );
    }

    #[test]
    fn kitty_shifted_punctuation_literals_send_text() {
        for ch in "!@#$%^&*()_+{}|:\"<>?~".chars() {
            let key = TerminalKey::new(KeyCode::Char(ch), KeyModifiers::SHIFT);
            let encoded = encode_terminal_key(key, KeyboardProtocol::from_kitty_flags(7));
            assert_eq!(encoded, ch.to_string().into_bytes(), "ch={ch}");
        }
    }

    #[test]
    fn kitty_shifted_punctuation_release_does_not_emit_text() {
        let key = TerminalKey::new(KeyCode::Char('?'), KeyModifiers::SHIFT)
            .with_kind(crossterm::event::KeyEventKind::Release);
        assert_eq!(
            encode_terminal_key(key, KeyboardProtocol::from_kitty_flags(7)),
            b"\x1b[63;2:3u"
        );
    }

    #[test]
    fn kitty_shifted_punctuation_does_not_infer_layout() {
        let key = TerminalKey::new(KeyCode::Char('1'), KeyModifiers::SHIFT);
        assert_eq!(
            encode_terminal_key(key, KeyboardProtocol::from_kitty_flags(7)),
            b"\x1b[49;2:1u"
        );
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
            let encoded = encode_terminal_key(key, KeyboardProtocol::from_kitty_flags(7));
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
            assert_eq!(
                encode_key(release, KeyboardProtocol::from_kitty_flags(1)),
                b""
            );
        }

        let modified_release = KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
            crossterm::event::KeyEventKind::Release,
        );
        assert_eq!(
            encode_key(modified_release, KeyboardProtocol::from_kitty_flags(3)),
            b"\x1b[13;5:3u"
        );
    }

    #[test]
    fn kitty_shifted_symbol_sends_text() {
        let key = TerminalKey::new(KeyCode::Char('1'), KeyModifiers::SHIFT)
            .with_shifted_codepoint('!' as u32);
        assert_eq!(
            encode_terminal_key(key, KeyboardProtocol::from_kitty_flags(7)),
            b"!"
        );
    }

    #[test]
    fn legacy_modified_special_roundtrip_matrix() {
        let cases = [
            KeyEvent::new(KeyCode::Up, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Down, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Insert, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Delete, KeyModifiers::ALT),
        ];

        for key in cases {
            let encoded = encode_key(key, KeyboardProtocol::legacy());
            let parsed = parse_terminal_key_sequence(
                std::str::from_utf8(&encoded).expect("test precondition"),
            )
            .expect("test precondition");
            assert_terminal_key_eq(&parsed, key.code, key.modifiers, key.kind, None);
        }
    }

    #[test]
    fn kitty_shifted_symbol_prefers_text_over_roundtrip_key_identity() {
        let key = TerminalKey::new(KeyCode::Char('1'), KeyModifiers::SHIFT)
            .with_shifted_codepoint('!' as u32);
        let encoded = encode_terminal_key(key, KeyboardProtocol::from_kitty_flags(7));
        assert_eq!(encoded, b"!");
    }

    #[test]
    fn legacy_basic_special_roundtrip_matrix() {
        let cases = [
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Up, KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Left, KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Right, KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Home, KeyModifiers::empty()),
            KeyEvent::new(KeyCode::End, KeyModifiers::empty()),
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::empty()),
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Insert, KeyModifiers::empty()),
            KeyEvent::new(KeyCode::Delete, KeyModifiers::empty()),
        ];

        for key in cases {
            let encoded = encode_key(key, KeyboardProtocol::legacy());
            let parsed = parse_terminal_key_sequence(
                std::str::from_utf8(&encoded).expect("test precondition"),
            )
            .expect("test precondition");
            assert_terminal_key_eq(&parsed, key.code, key.modifiers, key.kind, None);
        }
    }

    #[test]
    fn legacy_super_character_preserves_csi_u_chord() {
        let sequence = "\x1b[99;9u";
        let key = parse_terminal_key_sequence(sequence).expect("Super+C CSI-u key");

        assert_eq!(key.code, KeyCode::Char('c'));
        assert_eq!(key.modifiers, KeyModifiers::SUPER);
        assert_eq!(
            encode_terminal_key(key, KeyboardProtocol::legacy()),
            sequence.as_bytes()
        );
    }

    #[test]
    fn kitty_shifted_symbol_pair_matrix_is_encoded_as_text() {
        let cases = [('1', '!'), ('/', '?'), ('[', '{')];

        for (base, shifted) in cases {
            let key = TerminalKey::new(KeyCode::Char(base), KeyModifiers::SHIFT)
                .with_shifted_codepoint(shifted as u32);
            let encoded = encode_terminal_key(key, KeyboardProtocol::from_kitty_flags(7));
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
        let encoded = encode_terminal_key(key, KeyboardProtocol::from_kitty_flags(7));
        assert_eq!(encoded, "文".as_bytes());
    }

    #[test]
    fn kitty_functional_keys_use_legacy_compatible_forms_not_keypad_codes() {
        use crossterm::event::KeyEventKind;

        let cases = [
            (KeyCode::Up, "\x1b[1;1:1A"),
            (KeyCode::Down, "\x1b[1;1:1B"),
            (KeyCode::Right, "\x1b[1;1:1C"),
            (KeyCode::Left, "\x1b[1;1:1D"),
            (KeyCode::Home, "\x1b[1;1:1H"),
            (KeyCode::End, "\x1b[1;1:1F"),
            (KeyCode::Insert, "\x1b[2;1:1~"),
            (KeyCode::Delete, "\x1b[3;1:1~"),
            (KeyCode::PageUp, "\x1b[5;1:1~"),
            (KeyCode::PageDown, "\x1b[6;1:1~"),
            (KeyCode::F(1), "\x1b[1;1:1P"),
            (KeyCode::F(3), "\x1b[13;1:1~"),
            (KeyCode::F(12), "\x1b[24;1:1~"),
        ];
        for (code, expected) in cases {
            let key = TerminalKey::new(code, KeyModifiers::empty());
            let encoded = encode_terminal_key(key, KeyboardProtocol::from_kitty_flags(11));
            assert_eq!(encoded, expected.as_bytes(), "{code:?}");
            let parsed = parse_terminal_key_sequence(expected).expect("test precondition");
            assert_terminal_key_eq(
                &parsed,
                code,
                KeyModifiers::empty(),
                KeyEventKind::Press,
                None,
            );
        }

        let release = TerminalKey::new(KeyCode::Left, KeyModifiers::ALT | KeyModifiers::SHIFT)
            .with_kind(KeyEventKind::Release);
        let encoded = encode_terminal_key(release, KeyboardProtocol::from_kitty_flags(3));
        assert_eq!(encoded, b"\x1b[1;4:3D");
        for keypad_code in 57417..=57426 {
            assert!(!String::from_utf8_lossy(&encoded).contains(&keypad_code.to_string()));
        }
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
    fn mouse_events_are_filtered_by_protocol_mode() {
        use crossterm::event::MouseButton;

        let press = MouseEventKind::Down(MouseButton::Left);
        let release = MouseEventKind::Up(MouseButton::Left);
        let drag = MouseEventKind::Drag(MouseButton::Left);
        let sgr = MouseProtocolEncoding::Sgr;
        let none = KeyModifiers::empty();

        assert_eq!(
            encode_mouse_event(
                press,
                1,
                1,
                KeyModifiers::SHIFT,
                MouseProtocolMode::Press,
                sgr
            ),
            Some(b"\x1b[<0;1;1M".to_vec())
        );
        assert_eq!(
            encode_mouse_event(release, 1, 1, none, MouseProtocolMode::Press, sgr),
            None
        );
        assert_eq!(
            encode_mouse_event(release, 2, 3, none, MouseProtocolMode::PressRelease, sgr),
            Some(b"\x1b[<0;2;3m".to_vec())
        );
        assert_eq!(
            encode_mouse_event(drag, 2, 3, none, MouseProtocolMode::PressRelease, sgr),
            None
        );
        assert_eq!(
            encode_mouse_event(drag, 2, 3, none, MouseProtocolMode::ButtonMotion, sgr),
            Some(b"\x1b[<32;2;3M".to_vec())
        );
        assert_eq!(
            encode_mouse_event(
                MouseEventKind::Moved,
                2,
                3,
                none,
                MouseProtocolMode::ButtonMotion,
                sgr
            ),
            None
        );
        assert_eq!(
            encode_mouse_event(
                MouseEventKind::ScrollUp,
                48,
                139,
                none,
                MouseProtocolMode::AnyMotion,
                MouseProtocolEncoding::SgrPixels
            ),
            Some(b"\x1b[<64;48;139M".to_vec())
        );
        assert_eq!(
            encode_mouse_event(
                release,
                1,
                1,
                none,
                MouseProtocolMode::PressRelease,
                MouseProtocolEncoding::Default
            ),
            Some(vec![0x1b, b'[', b'M', 3 + 32, 33, 33])
        );
    }

    #[test]
    fn chinese_char_with_modifiers_falls_back_to_kitty_encoding() {
        let key = TerminalKey::new(KeyCode::Char('测'), KeyModifiers::ALT);
        let encoded = encode_terminal_key(key, KeyboardProtocol::from_kitty_flags(7));
        assert!(!encoded.is_empty());
        assert_ne!(encoded, "测".as_bytes());
    }
}
