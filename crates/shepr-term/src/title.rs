//! What a displayable title is: the one rule every layer applies to title text.
//!
//! A title is displayable when it holds no control character: not C0 (ESC, BEL,
//! CAN, SUB, CR, LF), DEL, nor a UTF-8-encoded C1 (U+009B CSI, U+0090 DCS,
//! U+009C ST), any of which would reach a terminal parser or garble a line.
//!
//! Length limits are not part of the rule. Each layer has its own reason for
//! its bound and passes it in: a parser's byte bound is a resource limit
//! (`shepr-vt`'s `MAX_TITLE_BYTES`, applied before any display text exists),
//! and a pane's retained title caps characters for what it keeps.

/// Whether `ch` may appear in a displayed title.
pub fn is_displayable_title_char(ch: char) -> bool {
    !ch.is_control()
}

/// `text` without its non-displayable characters.
pub fn strip_non_displayable(text: &str) -> String {
    text.chars()
        .filter(|ch| is_displayable_title_char(*ch))
        .collect()
}

/// `text` without its non-displayable characters, cut to `max_chars`
/// characters. The cap counts displayable characters, so stripped ones never
/// use it up.
pub fn sanitize_title(text: &str, max_chars: usize) -> String {
    text.chars()
        .filter(|ch| is_displayable_title_char(*ch))
        .take(max_chars)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_every_control_character() {
        assert_eq!(
            strip_non_displayable("a\x18b\x1ac\r\nd\x7fe\u{9b}f\u{90}g\u{85}h\u{9c}\x1b\x07\tí"),
            "abcdefghí"
        );
    }

    #[test]
    fn cap_counts_displayable_characters_only() {
        assert_eq!(sanitize_title("\x1bab\x07cd", 3), "abc");
        assert_eq!(sanitize_title("éé", 1), "é");
        assert_eq!(sanitize_title("abc", 0), "");
    }
}
