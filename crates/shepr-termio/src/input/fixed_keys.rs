//! Fixed key tables: keys the client routes itself in a mode or overlay
//! (copy mode, resize, the navigator and Help), as opposed to the configured
//! keybindings. One table per context maps keys to that context's commands;
//! the first binding that matches wins, and a binding in a help group lends
//! its key label to that group's help text.

use crossterm::event::{KeyCode, KeyModifiers};

use shepr_term::key::{KeyChord, TerminalKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModifierMatch {
    Any,
    Empty,
    Exact(KeyModifiers),
    Contains(KeyModifiers),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixedKey {
    /// A key code after config normalization (Shift+Tab reads as BackTab).
    Code(KeyCode, ModifierMatch),
    /// A key code exactly as reported.
    RawCode(KeyCode, ModifierMatch),
    /// The text a key types with at most Shift held, Shift applied.
    Character(char),
    /// A character key with Control in its modifiers.
    ControlCharacter(char, ModifierMatch),
}

impl FixedKey {
    pub fn matches(self, key: &TerminalKey) -> bool {
        match self {
            Self::Code(code, modifiers) => {
                let KeyChord {
                    code: actual_code,
                    modifiers: actual_modifiers,
                } = key.chord().normalized();
                actual_code == code && modifier_matches(actual_modifiers, modifiers)
            }
            Self::RawCode(code, modifiers) => {
                key.code == code && modifier_matches(key.modifiers, modifiers)
            }
            Self::Character(character) => {
                crate::copy_mode::copy_mode_key_char(key) == Some(character)
            }
            Self::ControlCharacter(character, modifiers) => {
                let KeyChord {
                    code: actual_code,
                    modifiers: actual_modifiers,
                } = key.chord().normalized();
                actual_code == KeyCode::Char(character)
                    && modifier_matches(actual_modifiers, modifiers)
            }
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::Code(code, _) | Self::RawCode(code, _) => match code {
                KeyCode::Esc => "esc".to_owned(),
                KeyCode::Enter => "enter".to_owned(),
                KeyCode::Up => "↑".to_owned(),
                KeyCode::Down => "↓".to_owned(),
                KeyCode::Left => "←".to_owned(),
                KeyCode::Right => "→".to_owned(),
                KeyCode::PageUp => "pgup".to_owned(),
                KeyCode::PageDown => "pgdn".to_owned(),
                KeyCode::Home => "home".to_owned(),
                KeyCode::End => "end".to_owned(),
                KeyCode::Backspace => "backspace".to_owned(),
                KeyCode::Delete => "delete".to_owned(),
                KeyCode::Char(' ') => "space".to_owned(),
                KeyCode::Char(character) => character.to_string(),
                _ => "key".to_owned(),
            },
            Self::Character(' ') => "space".to_owned(),
            Self::Character(character) => character.to_string(),
            Self::ControlCharacter(character, _) => format!("^{character}"),
        }
    }
}

fn modifier_matches(actual: KeyModifiers, expected: ModifierMatch) -> bool {
    match expected {
        ModifierMatch::Any => true,
        ModifierMatch::Empty => actual.is_empty(),
        ModifierMatch::Exact(modifiers) => actual == modifiers,
        ModifierMatch::Contains(modifiers) => actual.contains(modifiers),
    }
}

/// The help group type of a table whose help text is written out rather than
/// derived from the table, so no binding names a group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unlisted {}

#[derive(Clone, Copy, Debug)]
pub struct KeyBinding<Command, HelpGroup = Unlisted> {
    pub command: Command,
    pub key: FixedKey,
    pub help_group: Option<HelpGroup>,
}

impl<Command> KeyBinding<Command, Unlisted> {
    pub const fn unlisted(command: Command, key: FixedKey) -> Self {
        Self {
            command,
            key,
            help_group: None,
        }
    }
}

/// The command of the first binding that matches `key`.
pub fn command_for<Command: Copy, HelpGroup>(
    bindings: &[KeyBinding<Command, HelpGroup>],
    key: &TerminalKey,
) -> Option<Command> {
    bindings
        .iter()
        .find(|binding| binding.key.matches(key))
        .map(|binding| binding.command)
}

/// The labels of a help group's keys, in table order, joined by `separator`.
pub fn help_keys<Command, HelpGroup: Copy + Eq>(
    bindings: &[KeyBinding<Command, HelpGroup>],
    group: HelpGroup,
    separator: &str,
) -> String {
    bindings
        .iter()
        .filter(|binding| binding.help_group == Some(group))
        .map(|binding| binding.key.label())
        .collect::<Vec<_>>()
        .join(separator)
}
