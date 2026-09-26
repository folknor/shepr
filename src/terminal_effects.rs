use std::io::{self, Write};

/// Writes `OSC 0` to set the host terminal's window title; `None` resets it to
/// "shepr".
///
/// Every control character is dropped, not just the OSC terminators: titles
/// carry cwd and branch text, and a C0 (CAN/SUB abort the OSC, CR/LF garble
/// it), DEL or a UTF-8-encoded C1 (U+009B CSI, U+0090 DCS, U+009C ST) would
/// otherwise reach the host terminal's parser.
pub(crate) fn write_window_title<W: Write>(writer: &mut W, title: Option<&str>) -> io::Result<()> {
    let title = title.unwrap_or("shepr");
    let safe_title = title
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>();
    write!(writer, "\x1b]0;{safe_title}\x07")?;
    writer.flush()
}

fn osc52_sequence(bytes: &[u8]) -> String {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!("\x1b]52;c;{encoded}\x07")
}

/// Write clipboard bytes with native Linux tools when the host has a local
/// clipboard, falling back to an OSC 52 write through the host terminal.
///
/// Remote, VS Code remote, and WSL sessions use OSC 52 so bytes reach the
/// terminal on the user's machine. Some terminals still only honor BEL-
/// terminated writes, so OSC 52 uses BEL here.
pub(crate) fn write_clipboard_bytes(bytes: &[u8]) {
    if !crate::platform::prefers_osc52_clipboard() && crate::platform::write_clipboard(bytes) {
        return;
    }

    let sequence = osc52_sequence(bytes);
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(sequence.as_bytes());
    let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_sequence_uses_bel_terminator() {
        assert_eq!(osc52_sequence(b"hello"), "\x1b]52;c;aGVsbG8=\x07");
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
