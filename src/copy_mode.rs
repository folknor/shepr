use crossterm::event::{KeyCode, KeyModifiers};

use crate::input::TerminalKey;

/// Column of the first cell whose base character is not whitespace. Like
/// `last_character_col`, zero-width characters (combining marks, joiners,
/// variation selectors) belong to the cell before them: they neither take a
/// column nor start a cell, so a mark riding on a leading space does not make
/// that line "start" one column early.
pub(crate) fn first_non_blank_col(text: &str) -> Option<u16> {
    let mut col = 0u16;
    for (unit, width) in crate::ghostty::unicode_display_units(text) {
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

pub(crate) fn last_character_col(text: &str) -> Option<u16> {
    let mut col = 0u16;
    let mut last_col = None;
    for (_, width) in crate::ghostty::unicode_display_units(text) {
        let width = u16::from(width);
        if width > 0 {
            last_col = Some(col);
            col = col.saturating_add(width);
        }
    }
    last_col
}

pub(crate) fn copy_mode_page_lines(height: u16, half_page: bool) -> usize {
    if height <= 2 {
        1
    } else if half_page {
        usize::from(height / 2)
    } else {
        usize::from(height - 2)
    }
}

pub(crate) fn copy_mode_command_char(key: &TerminalKey) -> Option<char> {
    if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
        return None;
    }
    if let Some(ch) = key.shifted_codepoint.and_then(char::from_u32) {
        return Some(ch);
    }
    let KeyCode::Char(ch) = key.code else {
        return None;
    };
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        Some(shifted_ascii_char(ch).unwrap_or(ch))
    } else {
        Some(ch)
    }
}

fn shifted_ascii_char(ch: char) -> Option<char> {
    match ch {
        'a'..='z' => Some(ch.to_ascii_uppercase()),
        '1' => Some('!'),
        '2' => Some('@'),
        '3' => Some('#'),
        '4' => Some('$'),
        '5' => Some('%'),
        '6' => Some('^'),
        '7' => Some('&'),
        '8' => Some('*'),
        '9' => Some('('),
        '0' => Some(')'),
        '-' => Some('_'),
        '=' => Some('+'),
        '[' => Some('{'),
        ']' => Some('}'),
        '\\' => Some('|'),
        ';' => Some(':'),
        '\'' => Some('"'),
        ',' => Some('<'),
        '.' => Some('>'),
        '/' => Some('?'),
        '`' => Some('~'),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_non_blank_col_counts_cells_not_codepoints() {
        assert_eq!(first_non_blank_col("   foo"), Some(3));
        assert_eq!(first_non_blank_col("      "), None);
        // A wide first glyph starts where the blanks end.
        assert_eq!(first_non_blank_col("  界x"), Some(2));
        // An ideographic space is a two-column blank.
        assert_eq!(first_non_blank_col("\u{3000}x"), Some(2));
        // Joined emoji occupy the same two cells as in the terminal grid.
        assert_eq!(
            first_non_blank_col("   \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}x"),
            Some(3)
        );
        assert_eq!(
            last_character_col("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}x"),
            Some(2)
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
