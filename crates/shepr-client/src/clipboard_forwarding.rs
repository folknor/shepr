use super::*;

pub(super) fn decode_clipboard_payload(data: &str) -> Option<Vec<u8>> {
    use base64::Engine;

    base64::engine::general_purpose::STANDARD.decode(data).ok()
}

/// Decodes a server clipboard payload and writes it to the host clipboard.
/// `Err(InvalidData)` for a payload that is not base64; otherwise the outcome
/// of the native tool or OSC 52 fallback write.
pub(super) fn forward_clipboard(data: &str) -> io::Result<()> {
    let bytes = decode_clipboard_payload(data).ok_or_else(invalid_clipboard_payload)?;
    shepr_termio::host_term::title::write_clipboard_bytes(&bytes)
}

#[cfg(test)]
pub(super) fn forward_clipboard_with_writer<W: std::io::Write>(
    data: &str,
    writer: &mut W,
) -> io::Result<()> {
    let bytes = decode_clipboard_payload(data).ok_or_else(invalid_clipboard_payload)?;
    shepr_termio::host_term::title::write_clipboard_bytes_with_writer(&bytes, writer)
}

fn invalid_clipboard_payload() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "clipboard payload from the server is not valid base64",
    )
}
