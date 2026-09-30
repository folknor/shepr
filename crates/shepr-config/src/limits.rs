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

/// Maximum expanded input events accepted in one client pane-input message.
///
/// `shepr-protocol` re-exports this bound for the client batcher and the
/// server's input validation; it lives here, below the protocol, because
/// config also caps one mouse scroll step to it, since the server charges
/// scroll lines as expanded input work.
pub const MAX_INPUT_EVENT_BATCH: usize = 4096;

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

/// Maximum width or height in cells for a configured terminal grid or a
/// client-requested pane surface. The protocol reuses this limit so the
/// headless grid and attached-client grids share one server resource ceiling.
pub const MAX_TERMINAL_GRID_DIMENSION: u16 = 4096;

/// Maximum number of cells in a configured terminal grid or a
/// client-requested pane surface. The protocol reuses this limit so the
/// headless grid and attached-client grids share one server resource ceiling.
pub const MAX_TERMINAL_GRID_CELLS: usize = 1 << 22;

/// Return the cell count when a grid fits the shared terminal resource budget.
/// Zero dimensions remain representable here; callers that require a visible
/// terminal grid must check that separately.
pub fn terminal_grid_cells(cols: u16, rows: u16) -> Option<usize> {
    if cols > MAX_TERMINAL_GRID_DIMENSION || rows > MAX_TERMINAL_GRID_DIMENSION {
        return None;
    }
    let cells = usize::from(cols) * usize::from(rows);
    (cells <= MAX_TERMINAL_GRID_CELLS).then_some(cells)
}

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

/// Default blank rows between entries in expanded sidebars.
///
/// No blank rows keep the default sidebar compact; users can add spacing in config.
pub(crate) const DEFAULT_SIDEBAR_ROW_GAP: u16 = 0;

/// Maximum number of characters written to the outer terminal window title.
///
/// The title limit preserves long workspace and pane names while
/// bounding the control string sent to the terminal.
pub(crate) const MAX_WINDOW_TITLE_CHARS: usize = 200;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_grid_cells_enforces_shared_dimension_and_area_limits() {
        assert_eq!(
            terminal_grid_cells(4096, 1024),
            Some(MAX_TERMINAL_GRID_CELLS)
        );
        assert_eq!(terminal_grid_cells(4097, 1), None);
        assert_eq!(terminal_grid_cells(4096, 1025), None);
        assert_eq!(terminal_grid_cells(0, 24), Some(0));
    }
}
