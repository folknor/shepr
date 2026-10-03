//! Host input as a test writes it: whole byte strings rather than reads.

use shepr_termio::input::raw_input::{RawInputEvent, RawInputFramer};

/// Frame `data` as one complete read, then let the escape timeout expire, so
/// a lone or unfinished escape sequence resolves as it would after a pause.
pub fn parse_raw_input_bytes_sync(data: &[u8]) -> Vec<RawInputEvent> {
    let mut framer = RawInputFramer::default();
    let mut events = framer.push_framed(data);
    events.extend(framer.flush_timeout_framed());
    events.into_iter().map(|input| input.event).collect()
}

/// The pixel position of one complete SGR mouse report (`ESC [ < b ; x ; y`
/// then `M` or `m`), or `None` for anything else.
pub fn parse_sgr_mouse_report(data: &[u8]) -> Option<(u32, u32)> {
    let body = data.strip_prefix(b"\x1b[<")?;
    let body = body
        .strip_suffix(b"M")
        .or_else(|| body.strip_suffix(b"m"))?;
    let mut fields = body.split(|byte| *byte == b';');
    parse_number(fields.next()?)?;
    let x = parse_number(fields.next()?)?;
    let y = parse_number(fields.next()?)?;
    fields.next().is_none().then_some((x, y))
}

fn parse_number(value: &[u8]) -> Option<u32> {
    (!value.is_empty() && value.iter().all(u8::is_ascii_digit))
        .then(|| std::str::from_utf8(value).ok()?.parse().ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_accepts_only_complete_sgr_mouse_reports() {
        for (input, expected) in [
            (b"\x1b[<35;321;241M".as_slice(), Some((321, 241))),
            (b"\x1b[<0;1;2m".as_slice(), Some((1, 2))),
            (b"key".as_slice(), None),
            (b"\x1b[<0;1;2Mkey".as_slice(), None),
            (b"\x1b[<0;1M".as_slice(), None),
        ] {
            assert_eq!(parse_sgr_mouse_report(input), expected);
        }
    }
}
