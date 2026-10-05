const BRAILLE_RANGE: std::ops::RangeInclusive<char> = '\u{2800}'..='\u{28ff}';
const CLAUDE_TITLE_PREFIX_GLYPHS: &str = "·\u{2722}\u{2733}\u{2736}\u{273B}\u{273D}◐◓◑◒";

/// Whether `glyph` is a cross-agent title prefix that may be removed before
/// displaying a pane title. Membership identifies a removable marker, not an
/// agent or state; title cleanup is intentionally broader than Claude's
/// state-specific manifest rules (it takes the blank Braille cell and Claude's
/// idle marker too).
pub fn is_title_activity_glyph(glyph: char) -> bool {
    BRAILLE_RANGE.contains(&glyph) || CLAUDE_TITLE_PREFIX_GLYPHS.contains(glyph)
}

#[cfg(test)]
mod tests {
    use super::is_title_activity_glyph;

    #[test]
    fn title_prefix_cleanup_covers_its_full_marker_set() {
        for glyph in [
            '\u{2800}', '\u{28ff}', '·', '\u{2722}', '\u{2733}', '\u{2736}', '\u{273b}',
            '\u{273d}', '◐', '◓', '◑', '◒',
        ] {
            assert!(is_title_activity_glyph(glyph), "glyph={glyph:?}");
        }
        assert!(!is_title_activity_glyph('*'));
    }
}
