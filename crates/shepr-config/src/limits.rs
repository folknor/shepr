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
/// Its `usize` matches the raw config accessor, which stays wide for diagnostics.
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

/// Default pause between agent resumes when a restored session starts several
/// agents, so their startups do not contend for the machine at once.
pub(crate) const DEFAULT_STARTUP_PER_AGENT_DELAY: std::time::Duration =
    std::time::Duration::from_millis(100);

/// Maximum length in bytes of an SSH target.
///
/// Bounds a value that ends up on an ssh command line.
pub(crate) const MAX_SSH_TARGET_BYTES: usize = 1024;

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

/// Expanded sidebar width in columns while `ui.sidebar_width` is unset.
///
/// It sits inside the default sidebar bounds, so an unset width validates
/// unless the configured bounds exclude it.
pub(crate) const DEFAULT_SIDEBAR_WIDTH: u16 = 26;

/// Default blank rows between entries in expanded sidebars.
///
/// No blank rows keep the default sidebar compact; users can add spacing in config.
pub(crate) const DEFAULT_SIDEBAR_ROW_GAP: u16 = 0;

/// Minimum accepted mouse-wheel scroll step.
///
/// A zero-line step has no effect, so the setting requires a positive step.
pub(crate) const MIN_MOUSE_SCROLL_LINES: u16 = 1;

/// Lowest digit accepted for indexed workspace and agent bindings.
///
/// The first indexed shortcut matches the first visible item, keeping keys and
/// labels aligned.
pub(crate) const FIRST_INDEXED_BINDING_KEY: char = '1';

/// Highest digit accepted for indexed workspace and agent bindings.
///
/// The upper key keeps indexed shortcuts within the single-digit syntax.
pub(crate) const LAST_INDEXED_BINDING_KEY: char = '9';

/// Lowest supported function-key number in config key names.
///
/// Function-key numbering starts with the first function key, so zero is invalid.
pub(crate) const MIN_FUNCTION_KEY_NUMBER: u8 = 1;

/// Highest function-key number accepted by Crossterm's Unix keyboard parser.
///
/// Crossterm's Unix parser sets the upper bound because larger function-key
/// names cannot be represented by its key events.
pub(crate) const MAX_FUNCTION_KEY_NUMBER: u8 = 35;
