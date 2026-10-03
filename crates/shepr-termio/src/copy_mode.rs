use crossterm::event::{KeyCode, KeyModifiers};

use crate::input::fixed_keys::{FixedKey, KeyBinding, ModifierMatch, command_for, help_keys};
use shepr_term::key::TerminalKey;

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
    key.produced_char()
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_term::key::{KeyboardProtocol, encode_terminal_key};

    #[test]
    fn legacy_shift_ascii_punctuation_matches_copy_mode_mapping() {
        for (base, shifted) in [
            ('1', '!'),
            ('2', '@'),
            ('3', '#'),
            ('4', '$'),
            ('5', '%'),
            ('6', '^'),
            ('7', '&'),
            ('8', '*'),
            ('9', '('),
            ('0', ')'),
            ('-', '_'),
            ('=', '+'),
            ('[', '{'),
            (']', '}'),
            ('\\', '|'),
            (';', ':'),
            ('\'', '"'),
            (',', '<'),
            ('.', '>'),
            ('/', '?'),
            ('`', '~'),
        ] {
            let key = TerminalKey::new(KeyCode::Char(base), KeyModifiers::SHIFT);
            assert_eq!(
                copy_mode_key_char(&key),
                Some(shifted),
                "copy mode base={base}"
            );
            assert_eq!(
                encode_terminal_key(key, KeyboardProtocol::legacy()),
                shifted.to_string().as_bytes(),
                "base={base}"
            );
        }
    }
}
