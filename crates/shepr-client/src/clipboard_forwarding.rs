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
