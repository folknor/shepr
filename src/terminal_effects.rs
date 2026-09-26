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

#[cfg(test)]
mod tests {
    use super::*;

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
