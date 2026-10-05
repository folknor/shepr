//! Asks the terminal the preview runs in for its colours, with the query the
//! client sends at startup and the reply parsers it reads them with.

use std::io::{self, IsTerminal as _, Read as _, Write as _};
use std::sync::mpsc;
use std::time::Instant;

use shepr_term::host::TerminalTheme;
use shepr_termio::host_term::theme::{
    host_terminal_theme_query_sequence, parse_default_color_response, parse_palette_color_response,
};

use crate::limits::{QUERY_READ_CHUNK_BYTES, QUERY_REPLY_TIMEOUT};

/// Primary device attributes. Every terminal answers it, and in order, so its
/// reply arriving means every colour reply sent before it has arrived too.
const DEVICE_ATTRIBUTES_QUERY: &[u8] = b"\x1b[c";

/// The terminal's reported colours: what it answered before its device
/// attributes reply, or before the timeout on a terminal that never sends one.
pub(crate) fn query_terminal_theme() -> io::Result<TerminalTheme> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "stdin and stdout must be the terminal to query it; pass --background to preview a theme instead",
        ));
    }
    let _raw = RawMode::enable()?;
    let mut stdout = io::stdout();
    stdout.write_all(host_terminal_theme_query_sequence().as_bytes())?;
    stdout.write_all(DEVICE_ATTRIBUTES_QUERY)?;
    stdout.flush()?;

    // A blocking read cannot be timed out, so it runs on a thread of its own
    // that is left behind, still blocked, when the replies are in.
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut chunk = vec![0; QUERY_READ_CHUNK_BYTES];
        while let Ok(read) = stdin.read(&mut chunk) {
            if read == 0 || sender.send(chunk[..read].to_vec()).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + QUERY_REPLY_TIMEOUT;
    let mut replies = Vec::new();
    loop {
        let (theme, complete) = scan_replies(&replies);
        let remaining = deadline.saturating_duration_since(Instant::now());
        if complete || remaining.is_zero() {
            return Ok(theme);
        }
        match receiver.recv_timeout(remaining) {
            Ok(chunk) => replies.extend_from_slice(&chunk),
            Err(_) => return Ok(theme),
        }
    }
}

/// The colours in `replies` so far, and whether the device attributes reply
/// that ends them has arrived.
fn scan_replies(replies: &[u8]) -> (TerminalTheme, bool) {
    let mut theme = TerminalTheme::default();
    let mut rest = replies;
    while let Some(start) = rest.iter().position(|byte| *byte == 0x1b) {
        rest = &rest[start..];
        match rest.get(1) {
            Some(b']') => {
                let Some(end) = osc_end(rest) else {
                    break;
                };
                let sequence = String::from_utf8_lossy(&rest[..end]);
                if let Some((kind, color)) = parse_default_color_response(&sequence) {
                    theme = theme.with_color(kind, color);
                } else if let Some((index, color)) = parse_palette_color_response(&sequence) {
                    theme = theme.with_palette_color(index, color);
                }
                rest = &rest[end..];
            }
            Some(b'[') => {
                let Some(final_at) = rest[2..]
                    .iter()
                    .position(|byte| (0x40..=0x7e).contains(byte))
                    .map(|offset| offset + 2)
                else {
                    break;
                };
                if rest[final_at] == b'c' && rest.get(2) == Some(&b'?') {
                    return (theme, true);
                }
                rest = &rest[final_at + 1..];
            }
            _ => rest = &rest[1..],
        }
    }
    (theme, false)
}

/// The length of the OSC sequence `sequence` opens, terminator included, once
/// its BEL or ST terminator has arrived.
fn osc_end(sequence: &[u8]) -> Option<usize> {
    sequence
        .windows(2)
        .enumerate()
        .skip(2)
        .find_map(|(at, pair)| match pair {
            [0x07, _] => Some(at + 1),
            [0x1b, b'\\'] => Some(at + 2),
            _ => None,
        })
        .or_else(|| (sequence.last() == Some(&0x07)).then_some(sequence.len()))
}

/// Raw mode for as long as the guard lives, so the replies are not echoed and
/// arrive without waiting for a newline.
struct RawMode;

impl RawMode {
    fn enable() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        crossterm::terminal::disable_raw_mode().ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_term::RgbColor;

    #[test]
    fn collects_colour_replies_until_the_device_attributes_reply() {
        let replies = b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\\x1b]10;rgb:cdcd/d6d6/f4f4\x07\
            \x1b]4;2;rgb:a6a6/e3e3/a1a1\x1b\\";
        let (theme, complete) = scan_replies(replies);
        assert!(!complete);
        assert_eq!(
            theme.background,
            Some(RgbColor {
                r: 0x1e,
                g: 0x1e,
                b: 0x2e
            })
        );
        assert_eq!(
            theme.foreground,
            Some(RgbColor {
                r: 0xcd,
                g: 0xd6,
                b: 0xf4
            })
        );
        assert_eq!(
            theme.palette[2],
            Some(RgbColor {
                r: 0xa6,
                g: 0xe3,
                b: 0xa1
            })
        );

        let mut finished = replies.to_vec();
        finished.extend_from_slice(b"\x1b[?62;22c");
        assert_eq!(scan_replies(&finished), (theme, true));
    }

    #[test]
    fn an_unterminated_reply_waits_for_more_bytes() {
        let (theme, complete) = scan_replies(b"\x1b]11;rgb:1e1e/1e");
        assert!(!complete);
        assert_eq!(theme.background, None);
    }
}
