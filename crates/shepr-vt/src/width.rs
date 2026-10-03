//! Terminal grid and grapheme widths use distinct rules.
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::limits::MAX_UNICODE_CODEPOINT_WIDTH;

pub(super) fn is_halfwidth_voiced_mark(character: char) -> bool {
    matches!(character, '\u{ff9e}' | '\u{ff9f}')
}

/// U+FF9E/U+FF9F on their own. unicode-width measures them as zero-width, but
/// the terminal core gives them a cell (as wcwidth does).
pub fn is_halfwidth_katakana_voiced_mark(symbol: &str) -> bool {
    let mut characters = symbol.chars();
    let Some(mark) = characters.next() else {
        return false;
    };
    characters.next().is_none() && is_halfwidth_voiced_mark(mark)
}

/// A halfwidth katakana letter followed by its voiced mark: two columns in the
/// terminal core, although unicode-width measures the pair as one.
pub fn is_halfwidth_katakana_voiced_grapheme(symbol: &str) -> bool {
    let mut characters = symbol.chars();
    let Some(base) = characters.next() else {
        return false;
    };
    let Some(mark) = characters.next() else {
        return false;
    };
    characters.next().is_none()
        && ('\u{ff66}'..='\u{ff9d}').contains(&base)
        && is_halfwidth_voiced_mark(mark)
}

pub fn unicode_codepoint_width(character: char) -> u8 {
    if is_halfwidth_voiced_mark(character) {
        return 1;
    }
    u8::try_from(
        character
            .width()
            .unwrap_or(0)
            .min(usize::from(MAX_UNICODE_CODEPOINT_WIDTH)),
    )
    .unwrap_or(MAX_UNICODE_CODEPOINT_WIDTH)
}

/// Width of text under the terminal grid's per-codepoint and voiced-mark rules.
pub fn unicode_text_width(text: &str) -> usize {
    crate::unicode_display_units(text).fold(0usize, |width, (_, unit_width)| {
        width.saturating_add(usize::from(unit_width))
    })
}

/// Grapheme width with a column for each halfwidth voiced mark.
/// Grid text instead sums individual codepoints with [`unicode_text_width`].
pub fn unicode_grapheme_width(text: &str) -> usize {
    standard_grapheme_width(text).saturating_add(
        text.chars()
            .filter(|character| is_halfwidth_voiced_mark(*character))
            .count(),
    )
}

/// Unicode grapheme width before the terminal voiced-mark override. Cell
/// normalization uses this with explicit exceptions for terminal cell identity.
pub fn standard_grapheme_width(text: &str) -> usize {
    text.width()
}
