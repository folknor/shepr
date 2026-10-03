use crossterm::event::{KeyCode, KeyModifiers};

use crate::input::TerminalKey;
use crate::input::fixed_keys::{FixedKey, KeyBinding, ModifierMatch, command_for, help_keys};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyModeCommand {
    /// Clears the selection and search first; exits once neither remains.
    CancelOrClear,
    /// Exits copy mode whatever is selected or searched.
    Exit,
    Copy,
    BeginSelection,
    BeginLineSelection,
    MoveLeft,
    MoveDown,
    MoveUp,
    MoveRight,
    PageUp,
    PageDown,
    HalfPageUp,
    HalfPageDown,
    LineStart,
    LineEnd,
    HistoryStart,
    HistoryEnd,
    FirstNonBlank,
    SearchForward,
    SearchBackward,
    RepeatSearchForward,
    RepeatSearchBackward,
    WordNextStart,
    WordPreviousStart,
    WordNextEnd,
    BigWordNextStart,
    BigWordPreviousStart,
    BigWordNextEnd,
    ParagraphPrevious,
    ParagraphNext,
    SubmitSearch,
    CancelSearch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyModeHelpGroup {
    Cursor,
    Word,
    Paragraph,
    Search,
    Repeat,
    Selection,
    Copy,
    Exit,
    Clear,
    SearchPromptSubmit,
    SearchPromptCancel,
}

type Binding = KeyBinding<CopyModeCommand, CopyModeHelpGroup>;

use CopyModeCommand as Command;
use CopyModeHelpGroup as Help;

/// Any key code, whatever modifiers come with it.
const fn code(code: KeyCode) -> FixedKey {
    FixedKey::RawCode(code, ModifierMatch::Any)
}

const fn text(character: char) -> FixedKey {
    FixedKey::Character(character)
}

const fn control(character: char) -> FixedKey {
    FixedKey::ControlCharacter(character, ModifierMatch::Contains(KeyModifiers::CONTROL))
}

const fn binding(command: Command, key: FixedKey, help_group: Option<Help>) -> Binding {
    KeyBinding {
        command,
        key,
        help_group,
    }
}

const COPY_MODE_BINDINGS: &[Binding] = &[
    binding(Command::Exit, text('q'), Some(Help::Exit)),
    binding(
        Command::CancelOrClear,
        code(KeyCode::Esc),
        Some(Help::Clear),
    ),
    binding(Command::Copy, text('y'), Some(Help::Copy)),
    binding(Command::Copy, code(KeyCode::Enter), Some(Help::Copy)),
    binding(Command::MoveLeft, code(KeyCode::Left), None),
    binding(Command::MoveDown, code(KeyCode::Down), None),
    binding(Command::MoveUp, code(KeyCode::Up), None),
    binding(Command::MoveRight, code(KeyCode::Right), None),
    binding(Command::PageUp, code(KeyCode::PageUp), None),
    binding(Command::PageDown, code(KeyCode::PageDown), None),
    binding(Command::LineStart, code(KeyCode::Home), None),
    binding(Command::LineEnd, code(KeyCode::End), None),
    binding(Command::PageUp, control('b'), None),
    binding(Command::PageDown, control('f'), None),
    binding(Command::HalfPageUp, control('u'), None),
    binding(Command::HalfPageDown, control('d'), None),
    binding(Command::BeginSelection, text('v'), Some(Help::Selection)),
    binding(Command::BeginSelection, text(' '), Some(Help::Selection)),
    binding(Command::BeginLineSelection, text('V'), None),
    binding(Command::MoveLeft, text('h'), Some(Help::Cursor)),
    binding(Command::MoveDown, text('j'), Some(Help::Cursor)),
    binding(Command::MoveUp, text('k'), Some(Help::Cursor)),
    binding(Command::MoveRight, text('l'), Some(Help::Cursor)),
    binding(Command::HistoryStart, text('g'), None),
    binding(Command::HistoryEnd, text('G'), None),
    binding(Command::LineStart, text('0'), None),
    binding(Command::LineEnd, text('$'), None),
    binding(Command::FirstNonBlank, text('^'), None),
    binding(Command::SearchForward, text('/'), Some(Help::Search)),
    binding(Command::SearchBackward, text('?'), Some(Help::Search)),
    binding(Command::RepeatSearchForward, text('n'), Some(Help::Repeat)),
    binding(Command::RepeatSearchBackward, text('N'), Some(Help::Repeat)),
    binding(Command::WordNextStart, text('w'), Some(Help::Word)),
    binding(Command::WordPreviousStart, text('b'), Some(Help::Word)),
    binding(Command::WordNextEnd, text('e'), Some(Help::Word)),
    binding(Command::BigWordNextStart, text('W'), None),
    binding(Command::BigWordPreviousStart, text('B'), None),
    binding(Command::BigWordNextEnd, text('E'), None),
    binding(Command::ParagraphPrevious, text('{'), Some(Help::Paragraph)),
    binding(Command::ParagraphNext, text('}'), Some(Help::Paragraph)),
];

const COPY_MODE_PROMPT_BINDINGS: &[Binding] = &[
    binding(
        Command::CancelSearch,
        code(KeyCode::Esc),
        Some(Help::SearchPromptCancel),
    ),
    binding(
        Command::SubmitSearch,
        code(KeyCode::Enter),
        Some(Help::SearchPromptSubmit),
    ),
];

pub fn copy_mode_command(key: &TerminalKey) -> Option<CopyModeCommand> {
    command_for(COPY_MODE_BINDINGS, key)
}

pub fn copy_mode_prompt_command(key: &TerminalKey) -> Option<CopyModeCommand> {
    command_for(COPY_MODE_PROMPT_BINDINGS, key)
}

pub fn copy_mode_help_keys(group: CopyModeHelpGroup) -> String {
    let separator = if matches!(
        group,
        CopyModeHelpGroup::Search | CopyModeHelpGroup::Paragraph
    ) {
        " "
    } else {
        "/"
    };
    let bindings = if matches!(
        group,
        CopyModeHelpGroup::SearchPromptSubmit | CopyModeHelpGroup::SearchPromptCancel
    ) {
        COPY_MODE_PROMPT_BINDINGS
    } else {
        COPY_MODE_BINDINGS
    };
    help_keys(bindings, group, separator)
}

// These text-only helpers return a column, not a point, because their input
// has no absolute row. Copy motion joins the result to its cursor row in
// `shepr_vt::Point<AbsRow>`.
/// Column of the first cell whose base character is not whitespace. Like
/// `last_character_col`, zero-width characters (combining marks, joiners,
/// variation selectors) belong to the cell before them: they neither take a
/// column nor start a cell, so a mark riding on a leading space does not make
/// that line "start" one column early.
pub fn first_non_blank_col(text: &str) -> Option<u16> {
    let mut col = 0u16;
    for (unit, width) in shepr_vt::unicode_display_units(text) {
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
    for (_, width) in shepr_vt::unicode_display_units(text) {
        let width = u16::from(width);
        if width > 0 {
            last_col = Some(col);
            col = col.saturating_add(width);
        }
    }
    last_col
}

pub fn copy_mode_page_lines(height: u16, half_page: bool) -> usize {
    if height <= 2 {
        1
    } else if half_page {
        usize::from(height / 2)
    } else {
        usize::from(height - 2)
    }
}

/// The text value of an unmodified character key, after applying Shift.
/// `copy_mode_command` uses this in its character bindings; text input and
/// legacy terminal key encoding use the same conversion.
pub(crate) fn copy_mode_key_char(key: &TerminalKey) -> Option<char> {
    if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
        return None;
    }
    if let Some(ch) = key.shifted_codepoint {
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

/// Shift on a US-layout key. Copy-mode routing, key help, and the legacy key
/// encoder share this table so they read an unshifted key with Shift alike.
pub(crate) fn shifted_ascii_char(ch: char) -> Option<char> {
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
