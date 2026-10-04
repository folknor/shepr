//! Host terminal output for window titles and clipboard contents.

use std::io::{self, Write};

/// Writes `OSC 0` to set the host terminal's window title; `None` resets it to
/// "shepr".
///
/// The title is stripped by `shepr_term::title`, the one displayable-title
/// rule, with no length cap: the host writes whatever the server already
/// bounded. Every control character is dropped, not just the OSC terminators: titles
/// carry cwd and branch text, and a C0 (CAN/SUB abort the OSC, CR/LF garble
/// it), DEL or a UTF-8-encoded C1 (U+009B CSI, U+0090 DCS, U+009C ST) would
/// otherwise reach the host terminal's parser.
pub fn write_window_title<W: Write>(writer: &mut W, title: Option<&str>) -> io::Result<()> {
    let title = title.unwrap_or("shepr");
    let safe_title = shepr_term::title::strip_non_displayable(title);
    write!(writer, "\x1b]0;{safe_title}\x07")?;
    writer.flush()
}

/// One OSC 52 write per selection, the clipboard and then the primary
/// selection. A combined `cp` target is not used: terminals that honour only
/// one target read it differently, and two writes set both wherever each is
/// supported.
fn osc52_sequence(bytes: &[u8]) -> String {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!("\x1b]52;c;{encoded}\x07\x1b]52;p;{encoded}\x07")
}

/// Write clipboard bytes with the helpers `route` names when the host has a
/// local clipboard, falling back to an OSC 52 write through the host terminal.
/// Every copy sets both the clipboard and the primary selection, so text is
/// ready for middle-click paste and for Ctrl+V (and the clipboard sync of a
/// remote desktop session) alike.
///
/// Remote and VS Code remote sessions route to OSC 52 so bytes reach the
/// terminal on the user's machine. Some terminals still only honor BEL-
/// terminated writes, so OSC 52 uses BEL here.
///
/// `Ok` means the native tool took the bytes or the OSC 52 sequence was
/// written and flushed to the host terminal (whether the terminal honours
/// OSC 52 cannot be observed). `Err` means the copy did not happen; the caller
/// logs it with its own context. Clipboard bytes may contain private text, so
/// diagnostics should include only their byte count and error kind.
pub fn write_clipboard_bytes<W: Write>(
    bytes: &[u8],
    route: shepr_platform::ClipboardRoute,
    writer: &mut W,
) -> io::Result<()> {
    if route.write_with_helpers(bytes) {
        return Ok(());
    }
    write_osc52(bytes, writer)
}

fn write_osc52<W: Write>(bytes: &[u8], writer: &mut W) -> io::Result<()> {
    let sequence = osc52_sequence(bytes);
    writer
        .write_all(sequence.as_bytes())
        .and_then(|()| writer.flush())
        .map_err(|error| io::Error::from(error.kind()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_sequence_uses_bel_terminator() {
        assert_eq!(
            osc52_sequence(b"hello"),
            "\x1b]52;c;aGVsbG8=\x07\x1b]52;p;aGVsbG8=\x07"
        );
    }

    #[test]
    fn window_title_strips_terminators_and_defaults_to_shepr() {
        let mut output = Vec::new();
        write_window_title(&mut output, Some("shepr\x1b api\u{7}\u{9c}"))
            .expect("test precondition");
        assert_eq!(output, b"\x1b]0;shepr api\x07");

        output.clear();
        write_window_title(&mut output, None).expect("test precondition");
        assert_eq!(output, b"\x1b]0;shepr\x07");
    }

    #[test]
    fn window_title_strips_every_control_character() {
        let mut output = Vec::new();
        write_window_title(
            &mut output,
            Some("a\x18b\x1ac\r\nd\x7fe\u{9b}f\u{90}g\u{85}h\tí"),
        )
        .expect("test precondition");
        assert_eq!(output, "\x1b]0;abcdefghí\x07".as_bytes());
    }
}
