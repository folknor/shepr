use std::io;

/// Writes clipboard bytes from the server to the host clipboard.
/// The server sends bytes decoded from OSC 52 by its terminal parser.
pub(super) fn forward_clipboard(
    data: &[u8],
    route: shepr_platform::ClipboardRoute,
    writer: &mut impl io::Write,
) -> io::Result<()> {
    shepr_termio::host_term::title::write_clipboard_bytes(data, route, writer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_clipboard_writes_osc52_to_the_supplied_test_sink() {
        let mut output = Vec::new();
        forward_clipboard(b"test", shepr_platform::ClipboardRoute::Osc52, &mut output)
            .expect("clipboard bytes are written through OSC 52");
        assert_eq!(output, b"\x1b]52;c;dGVzdA==\x07");
    }
}
