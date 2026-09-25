use std::io::{self, Write};

pub(crate) fn write_window_title<W: Write>(writer: &mut W, title: Option<&str>) -> io::Result<()> {
    let title = title.unwrap_or("shepr");
    let safe_title = title
        .chars()
        .filter(|ch| !matches!(*ch, '\u{1b}' | '\u{7}' | '\u{9c}'))
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
        write_window_title(&mut output, Some("shepr\x1b api\u{7}\u{9c}")).expect("test precondition");
        assert_eq!(output, b"\x1b]0;shepr api\x07");

        output.clear();
        write_window_title(&mut output, None).expect("test precondition");
        assert_eq!(output, b"\x1b]0;shepr\x07");
    }
}
