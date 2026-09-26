use std::fmt::Write as _;

#[cfg(test)]
use crossterm::event::KeyEvent;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};

use super::model::KITTY_FLAG_REPORT_ALL_KEYS;
use super::{KeyboardProtocol, MouseProtocolEncoding, MouseProtocolMode, TerminalKey};

const KITTY_FLAG_REPORT_EVENT_TYPES: u16 = 0b0000_0010;
const KITTY_FLAG_REPORT_ALTERNATE_KEYS: u16 = 0b0000_0100;
const KITTY_FLAG_REPORT_ASSOCIATED_TEXT: u16 = 0b0001_0000;

/// Encode a key event for a PTY child using the pane's negotiated keyboard protocol.
/// Test-only: production keys go through `encode_terminal_key_with_modes`.
#[cfg(test)]
fn encode_key(key: KeyEvent, protocol: KeyboardProtocol) -> Vec<u8> {
    encode_terminal_key(key.into(), protocol)
}

pub fn encode_terminal_key(key: TerminalKey, protocol: KeyboardProtocol) -> Vec<u8> {
    // The host layout has not committed text for this Windows dead key. Neither
    // legacy nor Kitty panes should receive its physical character fallback.
    // Legacy Windows panes take the native ConPTY fallback before reaching this encoder.
    if key.is_windows_dead_key() {
        return Vec::new();
    }

    // Super has no legacy character encoding. Preserve the chord with CSI-u
    // instead of leaking the unmodified character into the pane.
    if matches!(protocol, KeyboardProtocol::Legacy)
        && key.kind != crossterm::event::KeyEventKind::Release
        && matches!(key.code, KeyCode::Char(_))
        && key.modifiers.contains(KeyModifiers::SUPER)
        && let Some(bytes) = try_encode_csi_u(&key, 0)
    {
        return bytes;
    }

    // REPORT_ALL_KEYS must retain physical press/repeat/release semantics instead of
    // reducing a native key to its layout-generated text.
    let preserve_physical_key = key.has_physical_identity() && protocol.reports_all_keys();
    if !preserve_physical_key
        && key.kind != crossterm::event::KeyEventKind::Release
        && let Some(text) = &key.generated_text
    {
        return text.as_bytes().to_vec();
    }

    // A release event only produces bytes when the pane protocol reports event
    // types (Kitty REPORT_EVENT_TYPES). Otherwise the child expects a single
    // legacy byte per keystroke, so re-emitting it on release would double keys
    // like Enter/Backspace. The Ghostty wrapper can route release events through
    // this fallback, so guard the fallback encoder too.
    if key.kind == crossterm::event::KeyEventKind::Release && !protocol.reports_event_types() {
        return Vec::new();
    }

    let kitty_first = protocol.reports_all_keys()
        || (key.kind == crossterm::event::KeyEventKind::Release && protocol.reports_event_types());

    if kitty_first
        && let KeyboardProtocol::Kitty { flags } = protocol
        && let Some(bytes) = try_encode_csi_u(&key, flags)
    {
        return bytes;
    }

    if let Some(bytes) = encode_text_input(&key) {
        return bytes;
    }

    if !kitty_first
        && let KeyboardProtocol::Kitty { flags } = protocol
        && let Some(bytes) = try_encode_csi_u(&key, flags)
    {
        return bytes;
    }
    if key.kind == crossterm::event::KeyEventKind::Release && protocol.reports_event_types() {
        return Vec::new();
    }
    encode_legacy(key)
}

/// Test-only: production applies DECCKM in `encode_terminal_key_with_modes`.
#[cfg(test)]
fn encode_cursor_key(code: KeyCode, application_cursor: bool) -> Vec<u8> {
    match (code, application_cursor) {
        (KeyCode::Up, true) => b"\x1bOA".to_vec(),
        (KeyCode::Down, true) => b"\x1bOB".to_vec(),
        (KeyCode::Right, true) => b"\x1bOC".to_vec(),
        (KeyCode::Left, true) => b"\x1bOD".to_vec(),
        (KeyCode::Up, false) => b"\x1b[A".to_vec(),
        (KeyCode::Down, false) => b"\x1b[B".to_vec(),
        (KeyCode::Right, false) => b"\x1b[C".to_vec(),
        (KeyCode::Left, false) => b"\x1b[D".to_vec(),
        _ => encode_legacy(KeyEvent::new(code, KeyModifiers::empty()).into()),
    }
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

/// `column` and `row` are the final 1-based coordinates to report.
fn encode_mouse_cb(
    base_button: u16,
    release: bool,
    column: u32,
    row: u32,
    modifiers: KeyModifiers,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    let mut cb = match (encoding, release) {
        (MouseProtocolEncoding::Sgr | MouseProtocolEncoding::SgrPixels, true) => base_button,
        (_, true) => 3,
        (_, false) => base_button,
    };
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
            let mut bytes = Vec::with_capacity(16);
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
    let mut buf = [0u8; 4];
    bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    Some(())
}

/// CSI u encoding: \e[{codepoint};{modifiers}u
/// Used when the child has pushed Kitty keyboard enhancement.
/// Returns None if the key doesn't need CSI u (unmodified basic keys).
fn try_encode_csi_u(key: &TerminalKey, flags: u16) -> Option<Vec<u8>> {
    let mods = key.modifiers;
    let event_suffix = kitty_event_suffix(key, flags);
    let report_all_keys = flags & KITTY_FLAG_REPORT_ALL_KEYS != 0;

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
    // Only use CSI u for character keys and keys without legacy representations.
    match key.code {
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::Home
        | KeyCode::End
        | KeyCode::PageUp
        | KeyCode::PageDown
        | KeyCode::Insert
        | KeyCode::Delete
        | KeyCode::F(_)
            if event_suffix.is_none() && !report_all_keys =>
        {
            return None; // let legacy handle these
        }
        _ => {}
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

    let mut sequence = String::with_capacity(32);
    sequence.push_str("\x1b[");
    write!(&mut sequence, "{codepoint}").ok()?;
    if let Some(shifted) = alternate_shifted {
        write!(&mut sequence, ":{shifted}").ok()?;
    }
    write!(&mut sequence, ";{modifier}").ok()?;
    if let Some(event) = event_suffix {
        write!(&mut sequence, ":{event}").ok()?;
    }
    if flags & KITTY_FLAG_REPORT_ASSOCIATED_TEXT != 0
        && let Some(text) = text_codepoint_for_key(key)
    {
        write!(&mut sequence, ";{text}").ok()?;
    }
    sequence.push('u');

    Some(sequence.into_bytes())
}

/// Kitty encoding of arrows, Home/End, Insert/Delete, PageUp/PageDown and
/// F1-F12: `CSI 1;mods[:event] {A,B,C,D,H,F,P,Q,S}` or
/// `CSI n;mods[:event] ~` (F3 is `CSI 13~` so it cannot be mistaken for a
/// cursor position report).
fn encode_kitty_functional_key(
    code: KeyCode,
    mods: KeyModifiers,
    event_suffix: Option<u8>,
) -> Option<Vec<u8>> {
    let (number, final_byte) = match code {
        KeyCode::Up => (1, 'A'),
        KeyCode::Down => (1, 'B'),
        KeyCode::Right => (1, 'C'),
        KeyCode::Left => (1, 'D'),
        KeyCode::Home => (1, 'H'),
        KeyCode::End => (1, 'F'),
        KeyCode::Insert => (2, '~'),
        KeyCode::Delete => (3, '~'),
        KeyCode::PageUp => (5, '~'),
        KeyCode::PageDown => (6, '~'),
        KeyCode::F(1) => (1, 'P'),
        KeyCode::F(2) => (1, 'Q'),
        KeyCode::F(3) => (13, '~'),
        KeyCode::F(4) => (1, 'S'),
        KeyCode::F(5) => (15, '~'),
        KeyCode::F(6) => (17, '~'),
        KeyCode::F(7) => (18, '~'),
        KeyCode::F(8) => (19, '~'),
        KeyCode::F(9) => (20, '~'),
        KeyCode::F(10) => (21, '~'),
        KeyCode::F(11) => (23, '~'),
        KeyCode::F(12) => (24, '~'),
        _ => return None,
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
    /// Active kitty keyboard flags (0 = legacy).
    pub kitty_flags: u16,
    /// xterm modifyOtherKeys level (0, 1 or 2).
    pub modify_other_keys: u8,
    /// DECCKM (mode 1): unmodified cursor keys use SS3.
    pub application_cursor: bool,
}

const KITTY_FLAG_DISAMBIGUATE: u16 = 0b0000_0001;

/// Encode a non-text key (Enter, Tab, arrows, function keys, ...) the way the
/// child negotiated: kitty flags first, then modifyOtherKeys, then legacy
/// xterm sequences honouring application cursor mode.
pub fn encode_terminal_key_with_modes(mut key: TerminalKey, modes: KeyEncodeModes) -> Vec<u8> {
    if modes.kitty_flags != 0 {
        // Kitty has no Backtab key; it is Tab with Shift.
        if key.code == KeyCode::BackTab {
            key.code = KeyCode::Tab;
            key.modifiers |= KeyModifiers::SHIFT;
        }
        // Disambiguation makes a bare Escape press unambiguous as CSI 27 u.
        if key.code == KeyCode::Esc
            && key.modifiers.is_empty()
            && key.kind != crossterm::event::KeyEventKind::Release
            && modes.kitty_flags & KITTY_FLAG_DISAMBIGUATE != 0
            && modes.kitty_flags & KITTY_FLAG_REPORT_EVENT_TYPES == 0
            && modes.kitty_flags & KITTY_FLAG_REPORT_ALL_KEYS == 0
        {
            return b"\x1b[27u".to_vec();
        }
        let bytes = encode_terminal_key(
            key.clone(),
            KeyboardProtocol::Kitty {
                flags: modes.kitty_flags,
            },
        );
        return apply_application_cursor(bytes, &key, modes.application_cursor);
    }

    if key.kind != crossterm::event::KeyEventKind::Release {
        if modes.modify_other_keys > 0
            && let Some(bytes) = encode_modify_other_keys(&key, modes.modify_other_keys)
        {
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

    let bytes = encode_terminal_key(key.clone(), KeyboardProtocol::Legacy);
    apply_application_cursor(bytes, &key, modes.application_cursor)
}

/// `CSI 27 ; mods ; code ~` for modified Enter/Tab/Backspace/Escape. Level 1
/// leaves Alt-only chords and Tab/Backspace (keys with well-known legacy
/// meanings) to the legacy encoder, as xterm does.
fn encode_modify_other_keys(key: &TerminalKey, level: u8) -> Option<Vec<u8>> {
    let code = match key.code {
        KeyCode::Enter => 13,
        KeyCode::Esc => 27,
        KeyCode::Tab if level >= 2 => 9,
        KeyCode::Backspace if level >= 2 => 127,
        _ => return None,
    };
    let mods = key.modifiers
        & (KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::SUPER);
    if mods.is_empty() || (level < 2 && mods == KeyModifiers::ALT) {
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
    let final_byte = match key.code {
        KeyCode::Up => b'A',
        KeyCode::Down => b'B',
        KeyCode::Right => b'C',
        KeyCode::Left => b'D',
        KeyCode::Home => b'H',
        KeyCode::End => b'F',
        _ => return bytes,
    };
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
pub(crate) fn encode_mouse_event(
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
        MouseProtocolMode::None => false,
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

    match code {
        // CSI 1;{mod}{letter} format
        KeyCode::Up => Some(format!("\x1b[1;{modifier}A").into_bytes()),
        KeyCode::Down => Some(format!("\x1b[1;{modifier}B").into_bytes()),
        KeyCode::Right => Some(format!("\x1b[1;{modifier}C").into_bytes()),
        KeyCode::Left => Some(format!("\x1b[1;{modifier}D").into_bytes()),
        KeyCode::Home => Some(format!("\x1b[1;{modifier}H").into_bytes()),
        KeyCode::End => Some(format!("\x1b[1;{modifier}F").into_bytes()),
        // CSI {n};{mod}~ format
        KeyCode::Insert => Some(format!("\x1b[2;{modifier}~").into_bytes()),
        KeyCode::Delete => Some(format!("\x1b[3;{modifier}~").into_bytes()),
        KeyCode::PageUp => Some(format!("\x1b[5;{modifier}~").into_bytes()),
        KeyCode::PageDown => Some(format!("\x1b[6;{modifier}~").into_bytes()),
        // F1-F4: CSI 1;{mod}{P-S}
        KeyCode::F(1) => Some(format!("\x1b[1;{modifier}P").into_bytes()),
        KeyCode::F(2) => Some(format!("\x1b[1;{modifier}Q").into_bytes()),
        KeyCode::F(3) => Some(format!("\x1b[1;{modifier}R").into_bytes()),
        KeyCode::F(4) => Some(format!("\x1b[1;{modifier}S").into_bytes()),
        // F5-F12: CSI {n};{mod}~
        KeyCode::F(n @ 5..=12) => {
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => unreachable!(),
            };
            Some(format!("\x1b[{code};{modifier}~").into_bytes())
        }
        _ => None,
    }
}

/// xterm modifier encoding: 1 + shift(1) + alt(2) + ctrl(4)
/// Used for legacy modified special keys (arrows, function keys, etc.)
fn xterm_modifier(mods: KeyModifiers) -> u32 {
    let mut m = 1u32;
    if mods.contains(KeyModifiers::SHIFT) {
        m += 1;
    }
    if mods.contains(KeyModifiers::ALT) {
        m += 2;
    }
    if mods.contains(KeyModifiers::CONTROL) {
        m += 4;
    }
    m
}

/// Kitty protocol modifier encoding: 1 + shift(1) + alt(2) + ctrl(4) + super(8) + hyper(16) + meta(32)
/// Superset of xterm - adds Super/Hyper/Meta bits.
fn kitty_modifier(mods: KeyModifiers) -> u32 {
    let mut m = xterm_modifier(mods);
    if mods.contains(KeyModifiers::SUPER) {
        m += 8;
    }
    if mods.contains(KeyModifiers::HYPER) {
        m += 16;
    }
    if mods.contains(KeyModifiers::META) {
        m += 32;
    }
    m
}

fn encode_text_input(key: &TerminalKey) -> Option<Vec<u8>> {
    let ch = text_char_for_key(key)?;
    let mut buf = [0u8; 4];
    Some(ch.encode_utf8(&mut buf).as_bytes().to_vec())
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

fn alternate_shifted_codepoint(key: &TerminalKey, flags: u16) -> Option<u32> {
    if flags & KITTY_FLAG_REPORT_ALTERNATE_KEYS == 0 {
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

fn kitty_event_suffix(key: &TerminalKey, flags: u16) -> Option<u8> {
    if flags & KITTY_FLAG_REPORT_EVENT_TYPES == 0 {
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
                let upper = ch.to_ascii_uppercase();
                match upper {
                    'A'..='Z' => vec![upper as u8 - 64],
                    ' ' | '@' | '2' => vec![0],
                    '[' | '3' => vec![27],
                    '\\' | '4' => vec![28],
                    ']' | '5' => vec![29],
                    '^' | '6' => vec![30],
                    '_' | '/' | '7' | '-' => vec![31],
                    _ => ch.to_string().into_bytes(),
                }
            } else {
                let ch = if key.modifiers == KeyModifiers::SHIFT {
                    shifted_text_char(key, ch).unwrap_or(ch)
                } else {
                    ch
                };
                let mut buf = [0u8; 4];
                ch.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![127],
        KeyCode::Tab => vec![9],
        KeyCode::BackTab => vec![27, 91, 90],
        KeyCode::Esc => vec![27],
        KeyCode::Left => vec![27, 91, 68],
        KeyCode::Right => vec![27, 91, 67],
        KeyCode::Up => vec![27, 91, 65],
        KeyCode::Down => vec![27, 91, 66],
        KeyCode::Home => vec![27, 91, 72],
        KeyCode::End => vec![27, 91, 70],
        KeyCode::PageUp => vec![27, 91, 53, 126],
        KeyCode::PageDown => vec![27, 91, 54, 126],
        KeyCode::Delete => vec![27, 91, 51, 126],
        KeyCode::Insert => vec![27, 91, 50, 126],
        KeyCode::F(n) => encode_f_key(n),
        _ => vec![],
    }
}

fn encode_f_key(n: u8) -> Vec<u8> {
    match n {
        1 => vec![27, 79, 80],
        2 => vec![27, 79, 81],
        3 => vec![27, 79, 82],
        4 => vec![27, 79, 83],
        5 => vec![27, 91, 49, 53, 126],
        6 => vec![27, 91, 49, 55, 126],
        7 => vec![27, 91, 49, 56, 126],
        8 => vec![27, 91, 49, 57, 126],
        9 => vec![27, 91, 50, 48, 126],
        10 => vec![27, 91, 50, 49, 126],
        11 => vec![27, 91, 50, 51, 126],
        12 => vec![27, 91, 50, 52, 126],
        _ => vec![],
    }
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
    fn kitty_all_keys_does_not_encode_windows_altgr_dead_key_phases() {
        use crossterm::event::KeyEventKind;

        let key = TerminalKey::new(KeyCode::Char('4'), KeyModifiers::empty()).with_windows_record(
            crate::input::WindowsKeyRecord {
                key_down: true,
                repeat_count: 1,
                virtual_key_code: 52,
                virtual_scan_code: 5,
                unicode: 0,
                control_key_state: 9,
            },
        );
        for kind in [
            KeyEventKind::Press,
            KeyEventKind::Repeat,
            KeyEventKind::Release,
        ] {
            assert!(
                encode_terminal_key(
                    key.clone().with_kind(kind),
                    KeyboardProtocol::Kitty { flags: 31 },
                )
                .is_empty(),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn legacy_enter() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::empty());
        assert_eq!(encode_key(key, KeyboardProtocol::Legacy), vec![b'\r']);
    }

    #[test]
    fn legacy_ctrl_c() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(encode_key(key, KeyboardProtocol::Legacy), vec![3]);
    }

    #[test]
    fn legacy_ctrl_slash_aliases_ctrl_underscore() {
        let key = KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL);
        assert_eq!(encode_key(key, KeyboardProtocol::Legacy), vec![31]);
    }

    #[test]
    fn legacy_ctrl_non_ascii_char_uses_utf8() {
        let key = KeyEvent::new(KeyCode::Char('ß'), KeyModifiers::CONTROL);
        assert_eq!(encode_key(key, KeyboardProtocol::Legacy), "ß".as_bytes());
    }

    #[test]
    fn legacy_shift_enter_is_just_cr() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, KeyboardProtocol::Legacy), vec![b'\r']);
    }

    #[test]
    fn legacy_alt_up() {
        let key = KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(encode_key(key, KeyboardProtocol::Legacy), b"\x1b[1;3A");
    }

    #[test]
    fn legacy_shift_right() {
        let key = KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, KeyboardProtocol::Legacy), b"\x1b[1;2C");
    }

    #[test]
    fn legacy_ctrl_left() {
        let key = KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL);
        assert_eq!(encode_key(key, KeyboardProtocol::Legacy), b"\x1b[1;5D");
    }

    #[test]
    fn legacy_ctrl_shift_end() {
        let key = KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, KeyboardProtocol::Legacy), b"\x1b[1;6F");
    }

    #[test]
    fn legacy_alt_delete() {
        let key = KeyEvent::new(KeyCode::Delete, KeyModifiers::ALT);
        assert_eq!(encode_key(key, KeyboardProtocol::Legacy), b"\x1b[3;3~");
    }

    #[test]
    fn legacy_shift_f5() {
        let key = KeyEvent::new(KeyCode::F(5), KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, KeyboardProtocol::Legacy), b"\x1b[15;2~");
    }

    #[test]
    fn legacy_alt_char_still_esc_prefix() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT);
        assert_eq!(encode_key(key, KeyboardProtocol::Legacy), b"\x1ba");
    }

    #[test]
    fn legacy_alt_shift_punctuation_uses_shifted_text() {
        let key = parse_terminal_key_sequence("\x1b[44:60;4u").expect("test precondition");
        assert_eq!(encode_terminal_key(key, KeyboardProtocol::Legacy), b"\x1b<");
    }

    #[test]
    fn legacy_alt_backspace_sends_escape_delete() {
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(encode_key(key, KeyboardProtocol::Legacy), b"\x1b\x7f");
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
    fn kitty_shift_enter() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(
            encode_key(key, KeyboardProtocol::Kitty { flags: 1 }),
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
            encode_key(key, KeyboardProtocol::Kitty { flags: 1 }),
            b"\x1b[97;6u"
        );
    }

    #[test]
    fn kitty_shift_uppercase_letter_sends_text() {
        let key = KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, KeyboardProtocol::Kitty { flags: 1 }), b"L");
    }

    #[test]
    fn kitty_shift_uppercase_letter_ignores_alternate_key_reporting_for_text() {
        let key = KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, KeyboardProtocol::Kitty { flags: 7 }), b"L");
    }

    #[test]
    fn kitty_shift_lowercase_letter_sends_uppercase_text() {
        let key = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, KeyboardProtocol::Kitty { flags: 1 }), b"L");
    }

    #[test]
    fn kitty_alt_shift_uppercase_letter_uses_base_codepoint() {
        let key = KeyEvent::new(KeyCode::Char('L'), KeyModifiers::ALT | KeyModifiers::SHIFT);
        assert_eq!(
            encode_key(key, KeyboardProtocol::Kitty { flags: 1 }),
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
            encode_key(key, KeyboardProtocol::Kitty { flags: 1 }),
            b"\x1b[108;6u"
        );
    }

    #[test]
    fn legacy_shift_uppercase_letter_stays_uppercase() {
        let key = KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT);
        assert_eq!(encode_key(key, KeyboardProtocol::Legacy), b"L");
    }

    #[test]
    fn kitty_alt_enter() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        assert_eq!(
            encode_key(key, KeyboardProtocol::Kitty { flags: 1 }),
            b"\x1b[13;3u"
        );
    }

    #[test]
    fn kitty_alt_backspace_uses_csi_u() {
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(
            encode_key(key, KeyboardProtocol::Kitty { flags: 1 }),
            b"\x1b[127;3u"
        );
    }

    #[test]
    fn kitty_plain_ctrl_c_uses_csi_u() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            encode_key(key, KeyboardProtocol::Kitty { flags: 1 }),
            b"\x1b[99;5u"
        );
    }

    #[test]
    fn kitty_plain_ctrl_c_includes_press_event_when_requested() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            encode_key(key, KeyboardProtocol::Kitty { flags: 3 }),
            b"\x1b[99;5:1u"
        );
    }

    #[test]
    fn kitty_unmodified_uses_legacy() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty());
        assert_eq!(encode_key(key, KeyboardProtocol::Kitty { flags: 1 }), b"a");
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
                encode_key(press, KeyboardProtocol::Kitty { flags: 3 }),
                expected,
                "{code:?} press should stay legacy-compatible without REPORT_ALL_KEYS"
            );

            let repeat = KeyEvent::new_with_kind(
                code,
                KeyModifiers::empty(),
                crossterm::event::KeyEventKind::Repeat,
            );
            assert_eq!(
                encode_key(repeat, KeyboardProtocol::Kitty { flags: 3 }),
                expected,
                "{code:?} repeat should stay legacy-compatible without REPORT_ALL_KEYS"
            );

            let release = KeyEvent::new_with_kind(
                code,
                KeyModifiers::empty(),
                crossterm::event::KeyEventKind::Release,
            );
            assert_eq!(
                encode_key(release, KeyboardProtocol::Kitty { flags: 3 }),
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
            encode_key(enter_press, KeyboardProtocol::Kitty { flags: 9 }),
            b"\x1b[13;1u"
        );

        let backspace_press = KeyEvent::new_with_kind(
            KeyCode::Backspace,
            KeyModifiers::empty(),
            crossterm::event::KeyEventKind::Press,
        );
        assert_eq!(
            encode_key(backspace_press, KeyboardProtocol::Kitty { flags: 11 }),
            b"\x1b[127;1:1u"
        );

        let backspace_release = KeyEvent::new_with_kind(
            KeyCode::Backspace,
            KeyModifiers::empty(),
            crossterm::event::KeyEventKind::Release,
        );
        assert_eq!(
            encode_key(backspace_release, KeyboardProtocol::Kitty { flags: 11 }),
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
                encode_key(key, KeyboardProtocol::Kitty { flags: 15 }),
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
                encode_terminal_key(key, KeyboardProtocol::Kitty { flags: 25 }),
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
                encode_terminal_key(key, KeyboardProtocol::Kitty { flags: 31 }),
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
            encode_key(release, KeyboardProtocol::Kitty { flags: 3 }),
            b"\x1b[106;1:3u"
        );

        let mut malformed_release = TerminalKey::from(release);
        malformed_release.generated_text = Some("j".to_owned());
        assert_eq!(
            encode_terminal_key(malformed_release, KeyboardProtocol::Kitty { flags: 3 }),
            b"\x1b[106;1:3u"
        );
    }

    #[test]
    fn kitty_shift_tab() {
        let key = KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT);
        assert_eq!(
            encode_key(key, KeyboardProtocol::Kitty { flags: 1 }),
            b"\x1b[9;2u"
        );
    }

    #[test]
    fn kitty_ctrl_shift_enter() {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        assert_eq!(
            encode_key(key, KeyboardProtocol::Kitty { flags: 1 }),
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
            encode_key(key, KeyboardProtocol::Kitty { flags: 3 }),
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
            encode_key(key, KeyboardProtocol::Kitty { flags: 7 }),
            b"\x1b[108:76;2:3u"
        );
    }

    #[test]
    fn kitty_shifted_punctuation_literals_send_text() {
        for ch in "!@#$%^&*()_+{}|:\"<>?~".chars() {
            let key = TerminalKey::new(KeyCode::Char(ch), KeyModifiers::SHIFT);
            let encoded = encode_terminal_key(key, KeyboardProtocol::Kitty { flags: 7 });
            assert_eq!(encoded, ch.to_string().into_bytes(), "ch={ch}");
        }
    }

    #[test]
    fn kitty_shifted_punctuation_release_does_not_emit_text() {
        let key = TerminalKey::new(KeyCode::Char('?'), KeyModifiers::SHIFT)
            .with_kind(crossterm::event::KeyEventKind::Release);
        assert_eq!(
            encode_terminal_key(key, KeyboardProtocol::Kitty { flags: 7 }),
            b"\x1b[63;2:3u"
        );
    }

    #[test]
    fn kitty_shifted_punctuation_does_not_infer_layout() {
        let key = TerminalKey::new(KeyCode::Char('1'), KeyModifiers::SHIFT);
        assert_eq!(
            encode_terminal_key(key, KeyboardProtocol::Kitty { flags: 7 }),
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
            let encoded = encode_terminal_key(key, KeyboardProtocol::Kitty { flags: 7 });
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
            // emit a byte on release, otherwise Enter/Backspace double (issue #769).
            assert_eq!(encode_key(release, KeyboardProtocol::Legacy), b"");
            assert_eq!(
                encode_key(release, KeyboardProtocol::Kitty { flags: 1 }),
                b""
            );
        }

        let modified_release = KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
            crossterm::event::KeyEventKind::Release,
        );
        assert_eq!(
            encode_key(modified_release, KeyboardProtocol::Kitty { flags: 3 }),
            b"\x1b[13;5:3u"
        );
    }

    #[test]
    fn kitty_shifted_symbol_sends_text() {
        let key = TerminalKey::new(KeyCode::Char('1'), KeyModifiers::SHIFT)
            .with_shifted_codepoint('!' as u32);
        assert_eq!(
            encode_terminal_key(key, KeyboardProtocol::Kitty { flags: 7 }),
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
            let encoded = encode_key(key, KeyboardProtocol::Legacy);
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
        let encoded = encode_terminal_key(key, KeyboardProtocol::Kitty { flags: 7 });
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
            let encoded = encode_key(key, KeyboardProtocol::Legacy);
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
            encode_terminal_key(key, KeyboardProtocol::Legacy),
            sequence.as_bytes()
        );
    }

    #[test]
    fn kitty_shifted_symbol_pair_matrix_is_encoded_as_text() {
        let cases = [('1', '!'), ('/', '?'), ('[', '{')];

        for (base, shifted) in cases {
            let key = TerminalKey::new(KeyCode::Char(base), KeyModifiers::SHIFT)
                .with_shifted_codepoint(shifted as u32);
            let encoded = encode_terminal_key(key, KeyboardProtocol::Kitty { flags: 7 });
            assert_eq!(encoded, shifted.to_string().into_bytes(), "base={base}");
        }
    }

    #[test]
    fn chinese_char_encodes_as_utf8() {
        let key = TerminalKey::new(KeyCode::Char('中'), KeyModifiers::empty());
        let encoded = encode_terminal_key(key, KeyboardProtocol::Legacy);
        assert_eq!(encoded, "中".as_bytes());
    }

    #[test]
    fn chinese_char_with_kitty_protocol_encodes_as_utf8() {
        let key = TerminalKey::new(KeyCode::Char('文'), KeyModifiers::empty());
        let encoded = encode_terminal_key(key, KeyboardProtocol::Kitty { flags: 7 });
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
            let encoded = encode_terminal_key(key, KeyboardProtocol::Kitty { flags: 11 });
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
        let encoded = encode_terminal_key(release, KeyboardProtocol::Kitty { flags: 3 });
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
            encode_terminal_key_with_modes(enter(KeyModifiers::SHIFT), level(1)),
            b"\x1b[27;2;13~"
        );
        assert_eq!(
            encode_terminal_key_with_modes(enter(KeyModifiers::SUPER), level(1)),
            b"\x1b[27;9;13~"
        );
        assert_eq!(
            encode_terminal_key_with_modes(enter(KeyModifiers::ALT), level(1)),
            b"\x1b\r"
        );
        assert_eq!(
            encode_terminal_key_with_modes(enter(KeyModifiers::ALT), level(2)),
            b"\x1b[27;3;13~"
        );
        assert_eq!(
            encode_terminal_key_with_modes(enter(KeyModifiers::SHIFT), level(0)),
            b"\r"
        );
        assert_eq!(
            encode_terminal_key_with_modes(
                TerminalKey::new(KeyCode::Backspace, KeyModifiers::CONTROL),
                level(0)
            ),
            b"\x08"
        );
    }

    #[test]
    fn mode_aware_encoder_maps_backtab_and_escape_under_kitty() {
        let kitty = KeyEncodeModes {
            kitty_flags: 1,
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
        assert_eq!(
            encode_mouse_event(press, 1, 1, none, MouseProtocolMode::None, sgr),
            None
        );
    }

    #[test]
    fn chinese_char_with_modifiers_falls_back_to_kitty_encoding() {
        let key = TerminalKey::new(KeyCode::Char('测'), KeyModifiers::ALT);
        let encoded = encode_terminal_key(key, KeyboardProtocol::Kitty { flags: 7 });
        assert!(!encoded.is_empty());
        assert_ne!(encoded, "测".as_bytes());
    }
}
