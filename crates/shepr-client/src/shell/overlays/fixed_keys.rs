use crossterm::event::{KeyCode, KeyModifiers};
use shepr_termio::input::TerminalKey;
use shepr_termio::input::fixed_keys::{FixedKey, KeyBinding, ModifierMatch, command_for};

// The navigator and Help footers are written out instead of derived from these
// tables, unlike the copy-mode and resize mode bars. They group keys more
// compactly than one label per binding can (`↑↓`, `ctrl+n/p`) and list only the
// keys worth naming, so the trailing close hint still fits a narrow overlay.
// The tests below check that every key a footer names routes to its command.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NavigatorCommand {
    BackOrClose,
    Open,
    MoveUp,
    MoveDown,
    MoveWorkspaceLeft,
    MoveWorkspaceRight,
    ClearFilter,
    Top,
    Bottom,
    Search,
    PageUp,
    PageDown,
    FilterBlocked,
    FilterWorking,
    FilterIdle,
    FilterAll,
}

const fn code(code: KeyCode, modifiers: ModifierMatch) -> FixedKey {
    FixedKey::Code(code, modifiers)
}

const fn control(character: char, modifiers: ModifierMatch) -> FixedKey {
    FixedKey::ControlCharacter(character, modifiers)
}

const CONTROL_ONLY: ModifierMatch = ModifierMatch::Exact(KeyModifiers::CONTROL);

const NAVIGATOR_MAIN_BINDINGS: &[KeyBinding<NavigatorCommand>] = &[
    KeyBinding::unlisted(
        NavigatorCommand::BackOrClose,
        code(KeyCode::Esc, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(
        NavigatorCommand::Open,
        code(KeyCode::Enter, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(
        NavigatorCommand::MoveUp,
        code(KeyCode::Up, ModifierMatch::Empty),
    ),
    KeyBinding::unlisted(
        NavigatorCommand::MoveDown,
        code(KeyCode::Down, ModifierMatch::Empty),
    ),
    KeyBinding::unlisted(NavigatorCommand::MoveUp, FixedKey::Character('k')),
    KeyBinding::unlisted(NavigatorCommand::MoveDown, FixedKey::Character('j')),
    KeyBinding::unlisted(
        NavigatorCommand::MoveWorkspaceLeft,
        code(KeyCode::Left, ModifierMatch::Empty),
    ),
    KeyBinding::unlisted(
        NavigatorCommand::MoveWorkspaceRight,
        code(KeyCode::Right, ModifierMatch::Empty),
    ),
    KeyBinding::unlisted(
        NavigatorCommand::ClearFilter,
        code(KeyCode::Backspace, ModifierMatch::Empty),
    ),
    KeyBinding::unlisted(
        NavigatorCommand::Top,
        code(KeyCode::Home, ModifierMatch::Empty),
    ),
    KeyBinding::unlisted(
        NavigatorCommand::Bottom,
        code(KeyCode::End, ModifierMatch::Empty),
    ),
    KeyBinding::unlisted(NavigatorCommand::Bottom, FixedKey::Character('G')),
    KeyBinding::unlisted(NavigatorCommand::Search, FixedKey::Character('/')),
    KeyBinding::unlisted(
        NavigatorCommand::PageDown,
        control('d', ModifierMatch::Contains(KeyModifiers::CONTROL)),
    ),
    KeyBinding::unlisted(
        NavigatorCommand::PageUp,
        control('u', ModifierMatch::Contains(KeyModifiers::CONTROL)),
    ),
    KeyBinding::unlisted(NavigatorCommand::FilterAll, FixedKey::Character('a')),
    KeyBinding::unlisted(NavigatorCommand::FilterBlocked, FixedKey::Character('b')),
    KeyBinding::unlisted(NavigatorCommand::FilterWorking, FixedKey::Character('w')),
    KeyBinding::unlisted(NavigatorCommand::FilterIdle, FixedKey::Character('i')),
];

const NAVIGATOR_SEARCH_BINDINGS: &[KeyBinding<NavigatorCommand>] = &[
    KeyBinding::unlisted(
        NavigatorCommand::BackOrClose,
        code(KeyCode::Esc, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(
        NavigatorCommand::Open,
        code(KeyCode::Enter, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(
        NavigatorCommand::MoveUp,
        code(KeyCode::Up, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(NavigatorCommand::MoveUp, control('p', CONTROL_ONLY)),
    KeyBinding::unlisted(
        NavigatorCommand::MoveDown,
        code(KeyCode::Down, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(NavigatorCommand::MoveDown, control('n', CONTROL_ONLY)),
];

pub(super) fn navigator_command_for_main(key: &TerminalKey) -> Option<NavigatorCommand> {
    command_for(NAVIGATOR_MAIN_BINDINGS, key)
}

pub(super) fn navigator_command_for_search(key: &TerminalKey) -> Option<NavigatorCommand> {
    command_for(NAVIGATOR_SEARCH_BINDINGS, key)
}

pub(super) fn navigator_footer(search_focused: bool) -> &'static str {
    if search_focused {
        " search type · move ↑↓/ctrl+n/p · open enter · back esc"
    } else {
        // Filters cover all agents or one of the three agent states; Ctrl+D pages by eight.
        " ↑↓/j/k rows · ←→ workspace · / search · a/b/w/i filter · enter open · esc close"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HelpCommand {
    Close,
    Back,
    Search,
    Top,
    Bottom,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    Edit,
}

const HELP_MAIN_BINDINGS: &[KeyBinding<HelpCommand>] = &[
    KeyBinding::unlisted(HelpCommand::Close, code(KeyCode::Enter, ModifierMatch::Any)),
    KeyBinding::unlisted(HelpCommand::Close, code(KeyCode::Esc, ModifierMatch::Any)),
    KeyBinding::unlisted(HelpCommand::Top, code(KeyCode::Home, ModifierMatch::Any)),
    KeyBinding::unlisted(HelpCommand::Bottom, code(KeyCode::End, ModifierMatch::Any)),
    KeyBinding::unlisted(HelpCommand::ScrollUp, code(KeyCode::Up, ModifierMatch::Any)),
    KeyBinding::unlisted(
        HelpCommand::ScrollDown,
        code(KeyCode::Down, ModifierMatch::Any),
    ),
    // Help scrolling accepts j and k with any modifiers, including Ctrl+J,
    // which raw-mode legacy input reports for LF.
    KeyBinding::unlisted(
        HelpCommand::ScrollUp,
        code(KeyCode::Char('k'), ModifierMatch::Any),
    ),
    KeyBinding::unlisted(
        HelpCommand::ScrollDown,
        code(KeyCode::Char('j'), ModifierMatch::Any),
    ),
    KeyBinding::unlisted(
        HelpCommand::PageUp,
        code(KeyCode::PageUp, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(
        HelpCommand::PageDown,
        code(KeyCode::PageDown, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(HelpCommand::Search, FixedKey::Character('/')),
    KeyBinding::unlisted(HelpCommand::Close, FixedKey::Character('?')),
];

const HELP_SEARCH_BINDINGS: &[KeyBinding<HelpCommand>] = &[
    KeyBinding::unlisted(HelpCommand::Close, code(KeyCode::Enter, ModifierMatch::Any)),
    KeyBinding::unlisted(HelpCommand::Back, code(KeyCode::Esc, ModifierMatch::Any)),
    KeyBinding::unlisted(HelpCommand::ScrollUp, code(KeyCode::Up, ModifierMatch::Any)),
    KeyBinding::unlisted(
        HelpCommand::ScrollDown,
        code(KeyCode::Down, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(HelpCommand::ScrollUp, control('p', CONTROL_ONLY)),
    KeyBinding::unlisted(HelpCommand::ScrollDown, control('n', CONTROL_ONLY)),
    KeyBinding::unlisted(
        HelpCommand::PageUp,
        code(KeyCode::PageUp, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(
        HelpCommand::PageDown,
        code(KeyCode::PageDown, ModifierMatch::Any),
    ),
    KeyBinding::unlisted(HelpCommand::Edit, code(KeyCode::Left, ModifierMatch::Empty)),
    KeyBinding::unlisted(
        HelpCommand::Edit,
        code(KeyCode::Right, ModifierMatch::Empty),
    ),
    KeyBinding::unlisted(HelpCommand::Edit, code(KeyCode::Home, ModifierMatch::Empty)),
    KeyBinding::unlisted(HelpCommand::Edit, code(KeyCode::End, ModifierMatch::Empty)),
    KeyBinding::unlisted(HelpCommand::Edit, control('u', CONTROL_ONLY)),
    KeyBinding::unlisted(HelpCommand::Edit, control('k', CONTROL_ONLY)),
    KeyBinding::unlisted(HelpCommand::Edit, control('y', CONTROL_ONLY)),
];

pub(super) fn help_command_for_main(key: &TerminalKey) -> Option<HelpCommand> {
    command_for(HELP_MAIN_BINDINGS, key)
}

pub(super) fn help_command_for_search(key: &TerminalKey) -> Option<HelpCommand> {
    command_for(HELP_SEARCH_BINDINGS, key)
}

pub(super) fn help_footer(search_focused: bool) -> &'static str {
    if search_focused {
        " edit ←→/home/end · kill ^u/^k · yank ^y · scroll ↑↓ · back esc"
    } else {
        " search / · scroll j/k/↑↓/pgup/pgdn · close esc/enter"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> TerminalKey {
        TerminalKey::new(code, KeyModifiers::NONE)
    }

    fn ctrl(character: char) -> TerminalKey {
        TerminalKey::new(KeyCode::Char(character), KeyModifiers::CONTROL)
    }

    fn char_key(character: char) -> TerminalKey {
        key(KeyCode::Char(character))
    }

    #[test]
    fn every_key_the_navigator_footers_name_routes_to_its_command() {
        use NavigatorCommand as C;
        let main = [
            (key(KeyCode::Up), C::MoveUp),
            (key(KeyCode::Down), C::MoveDown),
            (char_key('k'), C::MoveUp),
            (char_key('j'), C::MoveDown),
            (key(KeyCode::Left), C::MoveWorkspaceLeft),
            (key(KeyCode::Right), C::MoveWorkspaceRight),
            (char_key('/'), C::Search),
            (char_key('a'), C::FilterAll),
            (char_key('b'), C::FilterBlocked),
            (char_key('w'), C::FilterWorking),
            (char_key('i'), C::FilterIdle),
            (ctrl('d'), C::PageDown),
            (key(KeyCode::Enter), C::Open),
            (key(KeyCode::Esc), C::BackOrClose),
        ];
        for (pressed, command) in main {
            assert_eq!(navigator_command_for_main(&pressed), Some(command));
        }
        let search = [
            (key(KeyCode::Up), C::MoveUp),
            (key(KeyCode::Down), C::MoveDown),
            (ctrl('p'), C::MoveUp),
            (ctrl('n'), C::MoveDown),
            (key(KeyCode::Enter), C::Open),
            (key(KeyCode::Esc), C::BackOrClose),
        ];
        for (pressed, command) in search {
            assert_eq!(navigator_command_for_search(&pressed), Some(command));
        }
    }

    #[test]
    fn every_key_the_help_footers_name_routes_to_its_command() {
        use HelpCommand as C;
        let main = [
            (char_key('/'), C::Search),
            (char_key('j'), C::ScrollDown),
            (char_key('k'), C::ScrollUp),
            (key(KeyCode::Up), C::ScrollUp),
            (key(KeyCode::Down), C::ScrollDown),
            (key(KeyCode::PageUp), C::PageUp),
            (key(KeyCode::PageDown), C::PageDown),
            (key(KeyCode::Esc), C::Close),
            (key(KeyCode::Enter), C::Close),
        ];
        for (pressed, command) in main {
            assert_eq!(help_command_for_main(&pressed), Some(command));
        }
        let search = [
            (key(KeyCode::Left), C::Edit),
            (key(KeyCode::Right), C::Edit),
            (key(KeyCode::Home), C::Edit),
            (key(KeyCode::End), C::Edit),
            (ctrl('u'), C::Edit),
            (ctrl('k'), C::Edit),
            (ctrl('y'), C::Edit),
            (key(KeyCode::Up), C::ScrollUp),
            (key(KeyCode::Down), C::ScrollDown),
            (key(KeyCode::Esc), C::Back),
        ];
        for (pressed, command) in search {
            assert_eq!(help_command_for_search(&pressed), Some(command));
        }
    }
}
