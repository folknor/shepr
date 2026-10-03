//! Terminal grid and grapheme widths use distinct rules.
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::limits::MAX_UNICODE_CODEPOINT_WIDTH;

pub fn is_halfwidth_voiced_mark(character: char) -> bool {
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
    unicode_display_units(text).fold(0usize, |width, (_, unit_width)| {
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

/// Terminal column width of text under Ratatui's grapheme width rule, including the
/// halfwidth voiced marks terminals display in their own cells.
pub fn text_width(text: &str) -> usize {
    unicode_grapheme_width(text)
}

/// Unicode grapheme width before the terminal voiced-mark override. Cell
/// normalization uses this with explicit exceptions for terminal cell identity.
pub fn standard_grapheme_width(text: &str) -> usize {
    text.width()
}

/// A codepoint and any following zero-width codepoints stored in its cell.
///
/// This follows per-codepoint grid widths, not Unicode grapheme clusters.
/// Halfwidth voiced marks use the terminal-specific one-cell override in
/// [`unicode_codepoint_width`].
pub struct UnicodeDisplayUnits<'a> {
    text: &'a str,
    characters: std::str::CharIndices<'a>,
    next_character: Option<(usize, char, u8)>,
}

impl<'a> Iterator for UnicodeDisplayUnits<'a> {
    type Item = (&'a str, u8);

    fn next(&mut self) -> Option<Self::Item> {
        let (start, character, width) = match self.next_character.take() {
            Some(next) => next,
            None => {
                let (index, character) = self.characters.next()?;
                (index, character, unicode_codepoint_width(character))
            }
        };
        let first_len = character.len_utf8();
        let mut end = start + first_len;
        if !character.is_control() {
            for (index, following) in self.characters.by_ref() {
                let following_width = unicode_codepoint_width(following);
                if following.is_control() || following_width != 0 {
                    self.next_character = Some((index, following, following_width));
                    break;
                }
                end = index + following.len_utf8();
            }
        }
        let unit = &self.text[start..end];
        Some((unit, width))
    }
}

/// Iterate text by grid cells without allocating or using grapheme widths.
/// Each item starts with one codepoint and includes following zero-width
/// codepoints stored with it; a leading zero-width run has width zero.
pub fn unicode_display_units(text: &str) -> UnicodeDisplayUnits<'_> {
    UnicodeDisplayUnits {
        text,
        characters: text.char_indices(),
        next_character: None,
    }
}

// These text-only helpers return a column, not a point, because their input
// has no absolute row. Copy motion joins the result to its cursor row in
// `crate::Point<AbsRow>`.
/// Column of the first cell whose base character is not whitespace. Like
/// `last_character_col`, zero-width characters (combining marks, joiners,
/// variation selectors) belong to the cell before them: they neither take a
/// column nor start a cell, so a mark riding on a leading space does not make
/// that line "start" one column early.
pub fn first_non_blank_col(text: &str) -> Option<u16> {
    let mut col = 0u16;
    for (unit, width) in unicode_display_units(text) {
        let width = u16::from(width);
        if width == 0 {
            continue;
        }
        if !unit.chars().next().is_some_and(char::is_whitespace) {
            return Some(col);
        }
        col = col.saturating_add(width);
    }
    None
}

/// Column of the final occupied cell, or `None` when the text has no cells.
pub fn last_character_col(text: &str) -> Option<u16> {
    let mut col = 0u16;
    let mut last_col = None;
    for (_, width) in unicode_display_units(text) {
        let width = u16::from(width);
        if width > 0 {
            last_col = Some(col);
            col = col.saturating_add(width);
        }
    }
    last_col
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_width_matches_unicode_graphemes_and_terminal_voiced_marks() {
        assert_eq!(text_width("\u{2764}\u{fe0f}agent"), 7);
        assert_eq!(text_width("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}"), 2);
        assert_eq!(text_width("ｶﾞx"), 3);
        assert_eq!(text_width("aﾞ"), 2);
    }

    #[test]
    fn first_non_blank_col_counts_cells_not_codepoints() {
        assert_eq!(first_non_blank_col("   foo"), Some(3));
        assert_eq!(first_non_blank_col("      "), None);
        // A wide first glyph starts where the blanks end.
        assert_eq!(first_non_blank_col("  \u{754c}x"), Some(2));
        // An ideographic space is a two-column blank.
        assert_eq!(first_non_blank_col("\u{3000}x"), Some(2));
        // Grid width sums the emoji codepoint widths; joiners add no columns.
        assert_eq!(
            first_non_blank_col("   \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}x"),
            Some(3)
        );
        assert_eq!(
            last_character_col("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}x"),
            Some(6)
        );
    }

    #[test]
    fn first_non_blank_col_folds_zero_width_marks_into_their_cell() {
        // The combining acute rides on the leading space's cell; the first
        // non-blank cell is the `x` at column 2, not the mark "at" column 1.
        assert_eq!(first_non_blank_col(" \u{301} x"), Some(2));
        // A zero-width joiner on a blank does not take a column either.
        assert_eq!(first_non_blank_col(" \u{200d}x"), Some(1));
        // Agrees with last_character_col about where a lone glyph sits.
        assert_eq!(
            first_non_blank_col("  e\u{301}"),
            last_character_col("  e\u{301}")
        );
    }

    #[test]
    fn halfwidth_voiced_marks_take_their_terminal_columns() {
        assert_eq!(first_non_blank_col("  \u{ff9e}x"), Some(2));
        assert_eq!(last_character_col("x\u{ff9f}"), Some(1));
    }
}
