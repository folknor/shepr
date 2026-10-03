//! Mouse protocol vocabulary and child-facing mouse report encoding.

use crossterm::event::{KeyModifiers, MouseEventKind};
use shepr_core::limits::UTF8_MAX_BYTES_PER_CODEPOINT;

use crate::key::tables::{
    MOUSE_BUTTON_RELEASE, MOUSE_DRAG_OFFSET, mouse_button_code, mouse_modifier_bits,
    mouse_scroll_code,
};
use crate::limits::UTF8_MOUSE_REPORT_INITIAL_CAPACITY;

/// A pointer position reported to a pane: a cell, or pixels for SGR-pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Cell { column: u16, row: u16 },
    Pixels { x: u32, y: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseProtocolMode {
    Press,
    PressRelease,
    ButtonMotion,
    AnyMotion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEncoding {
    Default,
    Utf8,
    Sgr,
}

/// The mouse protocol selected by the child. `encoding` is used for cell
/// coordinates; pixel reports use SGR when `pixels_requested` is true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseProtocol {
    pub mode: MouseProtocolMode,
    pub encoding: MouseEncoding,
    pub pixels_requested: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseProtocolEncoding {
    Default,
    Utf8,
    Sgr,
    SgrPixels,
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
    let mut cb = if release && !sgr {
        u16::from(MOUSE_BUTTON_RELEASE)
    } else {
        base_button
    };
    cb += mouse_modifier_bits(modifiers);

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
    let (base_button, release) = match kind {
        MouseEventKind::Down(button) => (mouse_button_code(button)?, false),
        MouseEventKind::Up(button) => (mouse_button_code(button)?, true),
        MouseEventKind::Drag(button) => (mouse_button_code(button)? + MOUSE_DRAG_OFFSET, false),
        MouseEventKind::Moved => (u16::from(MOUSE_BUTTON_RELEASE) + MOUSE_DRAG_OFFSET, false),
        MouseEventKind::ScrollUp
        | MouseEventKind::ScrollDown
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => (mouse_scroll_code(kind)?, false),
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
        MouseEventKind::ScrollUp
        | MouseEventKind::ScrollDown
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => mouse_scroll_code(kind)?,
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
        MouseEventKind::Down(button) => (mouse_button_code(button)?, false),
        MouseEventKind::Up(button) => (mouse_button_code(button)?, true),
        MouseEventKind::Drag(button) => (mouse_button_code(button)? + MOUSE_DRAG_OFFSET, false),
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
    use super::*;

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
}
