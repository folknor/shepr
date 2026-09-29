/// Default approximate scrollback budget in bytes for each pane.
///
/// The default is a useful per-pane starting budget while remaining
/// predictable across a workspace; the configured value is converted to a
/// line count, with a separate minimum-line policy documented on the setting.
pub const DEFAULT_SCROLLBACK_LIMIT_BYTES: usize = 10_000_000;

/// Default number of pane lines moved by each mouse-wheel notch.
///
/// A small number of lines makes a wheel step useful without jumping a large part of the
/// visible history.
pub const DEFAULT_MOUSE_SCROLL_LINES: usize = 3;

/// Initial virtual terminal width when the server has no attached client.
///
/// This gives headless shells a conventional wide terminal before an
/// attached client's real geometry is available.
pub const DEFAULT_HEADLESS_COLS: u16 = 120;

/// Initial virtual terminal height when the server has no attached client.
///
/// This gives headless shells a useful multi-pane workspace before an
/// attached client's real geometry is available.
pub const DEFAULT_HEADLESS_ROWS: u16 = 40;

/// Maximum byte length of a session name.
///
/// Session names are ASCII path-safe identifiers; the byte cap bounds the name
/// while leaving room for descriptive names.
pub(crate) const MAX_SESSION_NAME_LEN: usize = 64;

/// Maximum rows accepted in each configured sidebar layout.
///
/// The row cap permits detailed layouts while bounding config-authored UI
/// structure.
pub(crate) const MAX_SIDEBAR_ROWS: usize = 16;

/// Maximum tokens accepted in one configured sidebar row.
///
/// The token cap allows detailed rows while keeping each row's
/// rendering work bounded.
pub(crate) const MAX_SIDEBAR_TOKENS_PER_ROW: usize = 16;

/// Maximum comparison rules accepted for one styled sidebar token.
///
/// The cap allows layered matching without letting one token carry an
/// unbounded rule list.
pub(crate) const MAX_SIDEBAR_RULES: usize = 16;

/// Default blank rows between entries in expanded sidebars.
///
/// No blank rows keep the default sidebar compact; users can add spacing in config.
pub(crate) const DEFAULT_SIDEBAR_ROW_GAP: u16 = 0;

/// Maximum byte length of a custom sidebar token name.
///
/// Custom names are ASCII identifiers, so this also caps their character
/// count; the cap permits readable names while keeping metadata compact.
pub(crate) const MAX_CUSTOM_SIDEBAR_TOKEN_NAME_BYTES: usize = 32;

/// Maximum number of characters written to the outer terminal window title.
///
/// The title limit preserves long workspace and pane names while
/// bounding the control string sent to the terminal.
pub(crate) const MAX_WINDOW_TITLE_CHARS: usize = 200;

/// Maximum number of entries accepted in the right side of the tab bar.
///
/// The cap leaves room for useful context while keeping one
/// config value from overwhelming the tab row.
pub(crate) const MAX_TAB_BAR_RIGHT_ENTRIES: usize = 16;

/// Default refresh interval in seconds for a tab-bar command entry.
///
/// This refreshes status often enough to feel current without
/// needlessly launching a command every render.
pub(crate) const DEFAULT_TAB_BAR_COMMAND_INTERVAL_SECONDS: u64 = 5;

/// Default timeout in seconds for a tab-bar command entry.
///
/// This allows ordinary status commands to finish while bounding how
/// long one command can hold its refresh slot.
pub(crate) const DEFAULT_TAB_BAR_COMMAND_TIMEOUT_SECONDS: u64 = 2;

/// Minimum accepted refresh interval in seconds for a tab-bar command.
///
/// Intervals are whole seconds and must be positive so a command cannot be
/// configured to refresh continuously.
pub(crate) const MIN_TAB_BAR_COMMAND_INTERVAL_SECONDS: u64 = 1;

/// Maximum accepted refresh interval in seconds for a tab-bar command.
///
/// The limit permits infrequent refreshes while placing a finite bound on the
/// interval value.
pub(crate) const MAX_TAB_BAR_COMMAND_INTERVAL_SECONDS: u64 = 365 * 24 * 60 * 60;

/// Minimum accepted timeout in seconds for a tab-bar command.
///
/// Timeouts are whole seconds and must be positive so each command has a
/// usable execution window.
pub(crate) const MIN_TAB_BAR_COMMAND_TIMEOUT_SECONDS: u64 = 1;

/// Maximum accepted timeout in seconds for a tab-bar command.
///
/// The ceiling allows a long status command while bounding a
/// stalled child process.
pub(crate) const MAX_TAB_BAR_COMMAND_TIMEOUT_SECONDS: u64 = 60 * 60;

/// Maximum supported mouse-wheel scroll step.
///
/// The validated runtime setting is stored as `NonZeroU16`, so this limit is
/// the largest value its representation can preserve.
pub(crate) const MAX_MOUSE_SCROLL_LINES: u16 = u16::MAX;

/// Minimum accepted mouse-wheel scroll step.
///
/// A zero-line step has no effect, so the setting requires a positive step.
pub(crate) const MIN_MOUSE_SCROLL_LINES: u16 = 1;

/// Lowest digit accepted for indexed workspace, tab, and agent bindings.
///
/// The first indexed shortcut matches the first visible item, keeping keys and
/// labels aligned.
pub(crate) const FIRST_INDEXED_BINDING_KEY: char = '1';

/// Highest digit accepted for indexed workspace, tab, and agent bindings.
///
/// The upper key keeps indexed shortcuts within the single-digit syntax.
pub(crate) const LAST_INDEXED_BINDING_KEY: char = '9';

/// Range token parsed by the keybinding config for all indexed digits.
///
/// Parser and help share this token so accepted shortcuts match the range they
/// display.
pub(crate) const INDEXED_BINDING_RANGE_SYNTAX: &str = "1..9";

/// Lowest supported function-key number in config key names.
///
/// Function-key numbering starts with the first function key, so zero is invalid.
pub(crate) const MIN_FUNCTION_KEY_NUMBER: u8 = 1;

/// Highest function-key number accepted by Crossterm's Unix keyboard parser.
///
/// Crossterm's Unix parser sets the upper bound because larger function-key
/// names cannot be represented by its key events.
pub(crate) const MAX_FUNCTION_KEY_NUMBER: u8 = 35;

macro_rules! count_key_binding_fields {
    (
        actions { $(($action_field:ident, $action_variant:ident, $action_default:literal, $action_group:literal, $action_label:literal, $action_doc:literal),)* }
        indexed { $(($indexed_field:ident, $indexed_variant:ident, $indexed_default:literal, $indexed_group:literal, $indexed_label:literal, $indexed_doc:literal, $indexed_help_after:literal),)* }
        navigate { $(($navigate_config_field:ident, $navigate_field:ident, $navigate_variant:ident, $navigate_default:literal, $navigate_group:literal, $navigate_label:literal, $navigate_doc:literal, $navigate_alias:ident),)* }
        navigate_indexed { $(($navigate_indexed_config_field:ident, $navigate_indexed_field:ident, $navigate_indexed_variant:ident, $navigate_indexed_default:literal, $navigate_indexed_group:literal, $navigate_indexed_label:literal, $navigate_indexed_doc:literal, $navigate_indexed_alias:ident),)* }
    ) => {
        [
            $(stringify!($action_field),)*
            $(stringify!($indexed_field),)*
            $(stringify!($navigate_config_field),)*
            $(stringify!($navigate_indexed_config_field),)*
        ].len()
    };
}

/// Number of keybinding fields in the wire config.
///
/// The value is derived from the shared keybinding table so the wire vector
/// accepts exactly the same set of fields as config parsing and presentation.
pub(crate) const KEY_BINDING_COUNT: usize = crate::keybinding_table!(count_key_binding_fields);
