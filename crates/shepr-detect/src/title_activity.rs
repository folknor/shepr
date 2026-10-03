/// Cross-agent title prefixes that may be removed before displaying a pane
/// title. Membership identifies a removable animation marker, not an agent or
/// state; each agent's manifest owns its narrower state-specific title rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TitleActivityGlyphs;

/// Shared set used when normalizing pane titles across agents.
pub const TITLE_ACTIVITY_GLYPHS: TitleActivityGlyphs = TitleActivityGlyphs;

impl TitleActivityGlyphs {
    const BRAILLE_RANGE: std::ops::RangeInclusive<char> = '\u{2800}'..='\u{28ff}';
    const CLAUDE_ANIMATION_GLYPHS: &str = "·\u{2722}\u{2733}\u{2736}\u{273B}\u{273D}◐◓◑◒";

    pub fn contains(&self, glyph: char) -> bool {
        Self::BRAILLE_RANGE.contains(&glyph) || Self::CLAUDE_ANIMATION_GLYPHS.contains(glyph)
    }
}
