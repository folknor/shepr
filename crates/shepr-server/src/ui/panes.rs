use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Borders, Paragraph, Wrap},
};

use super::PaneResizer;
use super::scrollbar::{render_pane_scrollbar, should_show_scrollbar};
use super::text::truncate_end;
use crate::app::AppState;
use shepr_mux::pane::{PaneRuntime, PaneRuntimeRegistry};
use shepr_mux::terminal::RestoreFailure;
use shepr_mux::workspace::{PaneChromeInfo as PaneInfo, pane_inner_rect};

pub(crate) fn pane_is_scrolled_back(rt: &PaneRuntime) -> bool {
    rt.scroll_metrics()
        .is_some_and(|metrics| metrics.offset_from_bottom > 0)
}

/// The unavailable-pane text: the guidance, then the error behind it on its
/// own line. Both borrow from the failure, so a redraw formats nothing.
fn restore_failure_text(failure: &RestoreFailure) -> Text<'_> {
    let mut lines = vec![Line::raw(failure.guidance())];
    if let Some(cause) = failure.cause() {
        lines.push(Line::from(vec![Span::raw("Error: "), Span::raw(cause)]));
    }
    Text::from(lines)
}

fn pane_border_title(label: &str, pane_width: u16, _focused: bool) -> Option<String> {
    let label = label.trim();
    if label.is_empty() || pane_width <= 4 {
        return None;
    }
    let max_label_width = pane_width.saturating_sub(4) as usize;
    Some(format!(" {} ", truncate_end(label, max_label_width)))
}

// Full view computation reaches this helper for active and background panes.
// Keep terminal queries narrow, allocation-free, and short under the core lock.
// The gutter rule itself is `terminal_content_rect`, shared with the size a
// new pane's PTY is spawned at, so the two cannot drift.
fn terminal_inner_rect(rt: &PaneRuntime, pane_inner: Rect, pane_scrollbars: bool) -> Rect {
    shepr_mux::workspace::terminal_content_rect(
        pane_inner,
        pane_scrollbars,
        pane_scrollbars && rt.alternate_screen_active(),
    )
}

fn stable_scrollbar_gutter(
    rt: &PaneRuntime,
    pane_inner: Rect,
    pane_scrollbars: bool,
) -> (Rect, Option<Rect>) {
    let inner_rect = terminal_inner_rect(rt, pane_inner, pane_scrollbars);
    if inner_rect == pane_inner {
        return (inner_rect, None);
    }
    let gutter = Rect::new(
        pane_inner.x + pane_inner.width.saturating_sub(1),
        pane_inner.y,
        1,
        pane_inner.height,
    );
    let scrollbar_rect = rt
        .scroll_metrics()
        .filter(|metrics| should_show_scrollbar(*metrics))
        .map(|_| gutter);

    (inner_rect, scrollbar_rect)
}

/// Apply a computed pane layout to every runtime it contains.
pub(super) fn resize_pane_infos(
    app: &AppState,
    resizer: &PaneResizer<'_>,
    ws_idx: usize,
    pane_infos: &[PaneInfo],
    cell_size: shepr_termio::host_term::cell_size::HostCellSize,
) {
    let Some(workspace) = app.workspaces.get(ws_idx) else {
        return;
    };

    for info in pane_infos {
        let Some(terminal_id) = workspace.terminal_id(info.id) else {
            continue;
        };
        let Some(rt) = resizer.runtime(terminal_id) else {
            continue;
        };
        rt.resize(shepr_core::geometry::PaneGeometry::new(
            info.inner_rect.width,
            info.inner_rect.height,
            cell_size.width_px,
            cell_size.height_px,
        ));
    }
}

/// Compute pane layout info without mutating pane runtimes.
pub(super) fn compute_pane_infos_for_workspace(
    app: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
    ws_idx: usize,
    area: Rect,
) -> Vec<PaneInfo> {
    let Some(workspace) = app.workspaces.get(ws_idx) else {
        return Vec::new();
    };

    let mut pane_infos = app
        .pane_geometry_in(area)
        .visible_panes(workspace.layout(), workspace.zoomed());

    for info in &mut pane_infos {
        let pane_inner = pane_inner_rect(info.rect, info.borders);

        // A pane without a runtime (one waiting on agent resume, or whose
        // restore failed) gets the content rect of a fresh shell on the
        // primary screen: the gutter reserved, as the runtime it starts will
        // have it. Its resume is spawned at exactly this size, so its first
        // resize does not change it.
        let (inner_rect, scrollbar_rect) =
            match app.runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, info.id) {
                Some(rt) => stable_scrollbar_gutter(rt, pane_inner, app.settings.pane_scrollbars),
                None => (
                    shepr_mux::workspace::terminal_content_rect(
                        pane_inner,
                        app.settings.pane_scrollbars,
                        false,
                    ),
                    None,
                ),
            };

        info.inner_rect = inner_rect;
        info.scrollbar_rect = scrollbar_rect;
    }

    pane_infos
}

pub(super) fn render_panes(
    app: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
    frame: &mut Frame<'_>,
    target: Option<&shepr_protocol::WorkspaceId>,
    pane_infos: &[PaneInfo],
    split_borders: &[shepr_core::layout::SplitBorder],
) {
    let Some(target) = target else {
        return;
    };
    let Some(ws_idx) = app.workspace_index(target) else {
        return;
    };
    let Some(ws) = app.workspaces.get(ws_idx) else {
        return;
    };

    for info in pane_infos {
        if let Some(rt) = app.runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, info.id) {
            let show_cursor = info.is_focused && !pane_is_scrolled_back(rt);
            rt.render(frame, info.inner_rect, show_cursor);
            render_pane_scrollbar(app, frame, info, rt);
        } else if let Some(reason) = ws
            .terminal_id(info.id)
            .and_then(|id| app.terminals.get(id))
            .and_then(|terminal| terminal.restore_error.as_ref())
        {
            frame.render_widget(
                Paragraph::new(restore_failure_text(reason)).wrap(Wrap { trim: false }),
                info.inner_rect,
            );
        }
    }

    render_pane_borders(app, ws, pane_infos, split_borders, frame);
}

#[derive(Clone, Copy, Default)]
struct LineCell {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
}

fn render_pane_borders(
    app: &AppState,
    ws: &shepr_mux::workspace::Workspace,
    pane_infos: &[PaneInfo],
    split_borders: &[shepr_core::layout::SplitBorder],
    frame: &mut Frame<'_>,
) {
    if !app.settings.pane_borders.draws_borders()
        || pane_infos.iter().all(|info| info.borders.is_empty())
    {
        return;
    }

    let mut cells = std::collections::HashMap::<(u16, u16), LineCell>::new();
    for info in pane_infos {
        add_pane_border_cells(&mut cells, info);
    }
    add_split_border_cells(app.settings.pane_gaps, split_borders, &mut cells);

    let buf = frame.buffer_mut();
    let area = buf.area;
    for ((x, y), line) in cells {
        if x < area.x
            || x >= area.x.saturating_add(area.width)
            || y < area.y
            || y >= area.y.saturating_add(area.height)
        {
            continue;
        }
        let focused = pane_infos
            .iter()
            .any(|info| info.is_focused && line_touches_pane(x, y, info, app.settings.pane_gaps));
        let symbol = line_cell_symbol(line);
        if symbol.is_empty() {
            continue;
        }
        let cell = &mut buf[(x, y)];
        cell.set_symbol(symbol);
        let color = if focused {
            app.settings.palette.accent
        } else {
            app.settings.palette.overlay0
        };
        cell.set_style(Style::default().fg(color));
    }

    render_pane_border_titles(app, ws, pane_infos, frame);
}

fn add_split_border_cells(
    pane_gaps: bool,
    split_borders: &[shepr_core::layout::SplitBorder],
    cells: &mut std::collections::HashMap<(u16, u16), LineCell>,
) {
    if pane_gaps {
        return;
    }

    for split in split_borders {
        match split.direction {
            shepr_core::layout::Direction::Horizontal => {
                let x = split.pos;
                let end = split.area.y.saturating_add(split.area.height);
                for y in split.area.y..=end {
                    if !cells.contains_key(&(x, y)) {
                        continue;
                    }
                    let left = x
                        .checked_sub(1)
                        .and_then(|left_x| cells.get(&(left_x, y)))
                        .is_some_and(|cell| cell.left || cell.right);
                    let right = cells
                        .get(&(x.saturating_add(1), y))
                        .is_some_and(|cell| cell.left || cell.right);
                    let cell = cells.entry((x, y)).or_default();
                    cell.up |= y > split.area.y;
                    cell.down |= y + 1 < end;
                    cell.left |= left;
                    cell.right |= right;
                }
            }
            shepr_core::layout::Direction::Vertical => {
                let y = split.pos;
                let end = split.area.x.saturating_add(split.area.width);
                for x in split.area.x..=end {
                    if !cells.contains_key(&(x, y)) {
                        continue;
                    }
                    let up = y
                        .checked_sub(1)
                        .and_then(|up_y| cells.get(&(x, up_y)))
                        .is_some_and(|cell| cell.up || cell.down);
                    let down = cells
                        .get(&(x, y.saturating_add(1)))
                        .is_some_and(|cell| cell.up || cell.down);
                    let cell = cells.entry((x, y)).or_default();
                    cell.left |= x > split.area.x;
                    cell.right |= x + 1 < end;
                    cell.up |= up;
                    cell.down |= down;
                }
            }
        }
    }
}

fn add_pane_border_cells(
    cells: &mut std::collections::HashMap<(u16, u16), LineCell>,
    info: &PaneInfo,
) {
    let rect = info.rect;
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let right = rect.x.saturating_add(rect.width).saturating_sub(1);
    let bottom = rect.y.saturating_add(rect.height).saturating_sub(1);

    if info.borders.contains(Borders::TOP) {
        for x in rect.x..=right {
            let cell = cells.entry((x, rect.y)).or_default();
            cell.left |= x > rect.x;
            cell.right |= x < right;
        }
    }
    if info.borders.contains(Borders::BOTTOM) {
        for x in rect.x..=right {
            let cell = cells.entry((x, bottom)).or_default();
            cell.left |= x > rect.x;
            cell.right |= x < right;
        }
    }
    if info.borders.contains(Borders::LEFT) {
        for y in rect.y..=bottom {
            let cell = cells.entry((rect.x, y)).or_default();
            cell.up |= y > rect.y;
            cell.down |= y < bottom;
        }
    }
    if info.borders.contains(Borders::RIGHT) {
        for y in rect.y..=bottom {
            let cell = cells.entry((right, y)).or_default();
            cell.up |= y > rect.y;
            cell.down |= y < bottom;
        }
    }
}

fn line_touches_pane(x: u16, y: u16, info: &PaneInfo, pane_gaps: bool) -> bool {
    let rect = info.rect;
    if rect.width == 0 || rect.height == 0 {
        return false;
    }
    let right = rect.x.saturating_add(rect.width).saturating_sub(1);
    let bottom = rect.y.saturating_add(rect.height).saturating_sub(1);
    let in_rows = y >= rect.y && y <= bottom;
    let in_cols = x >= rect.x && x <= right;
    let own_border =
        (in_rows && (x == rect.x || x == right)) || (in_cols && (y == rect.y || y == bottom));

    if pane_gaps {
        return own_border;
    }

    let shared_right = rect.x.saturating_add(rect.width);
    let shared_bottom = rect.y.saturating_add(rect.height);
    own_border
        || (in_rows && x == shared_right)
        || (in_cols && y == shared_bottom)
        || (x == shared_right && y == shared_bottom)
}

fn render_pane_border_titles(
    app: &AppState,
    ws: &shepr_mux::workspace::Workspace,
    pane_infos: &[PaneInfo],
    frame: &mut Frame<'_>,
) {
    let buf = frame.buffer_mut();
    let area = buf.area;
    for info in pane_infos {
        if !info.borders.contains(Borders::TOP) || info.rect.width <= 4 {
            continue;
        }
        let Some(title) = ws
            .pane_state(info.id)
            .and_then(|pane| app.terminals.get(&pane.attached_terminal_id))
            .and_then(|terminal| {
                terminal.border_label(app.settings.show_agent_labels_on_pane_borders)
            })
            .and_then(|label| pane_border_title(&label, info.rect.width, info.is_focused))
        else {
            continue;
        };
        let y = info.rect.y;
        if y < area.y || y >= area.y.saturating_add(area.height) {
            continue;
        }
        let start_x = info.rect.x.saturating_add(1);
        let end_x = info
            .rect
            .x
            .saturating_add(info.rect.width)
            .saturating_sub(1)
            .min(area.x.saturating_add(area.width));
        if start_x >= end_x {
            continue;
        }
        let color = if info.is_focused {
            app.settings.palette.accent
        } else {
            app.settings.palette.overlay0
        };
        let mut style = Style::default().fg(color);
        if info.is_focused {
            style = style.add_modifier(Modifier::BOLD);
        }
        buf.set_stringn(
            start_x,
            y,
            title,
            end_x.saturating_sub(start_x) as usize,
            style,
        );
    }
}

fn line_cell_symbol(line: LineCell) -> &'static str {
    match (line.up, line.down, line.left, line.right) {
        (true, true, true, true) => "┼",
        (true, true, true, false) => "┤",
        (true, true, false, true) => "├",
        (true, false, true, true) => "┴",
        (false, true, true, true) => "┬",
        (true, _, false, false) | (false, true, false, false) => "│",
        (false, false, true, _) | (false, false, false, true) => "─",
        (false, true, false, true) => "┌",
        (false, true, true, false) => "┐",
        (true, false, false, true) => "└",
        (true, false, true, false) => "┘",
        _ => "",
    }
}

#[cfg(test)]
use super::text::display_width;

#[cfg(test)]
use shepr_mux::workspace::apply_pane_chrome;

#[cfg(test)]
fn compute_pane_infos(
    app: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
    area: Rect,
) -> Vec<PaneInfo> {
    let Some(workspace_index) = app.active_index() else {
        return Vec::new();
    };
    compute_pane_infos_for_workspace(app, terminal_runtimes, workspace_index, area)
}

#[cfg(test)]
use ratatui::{buffer::Buffer, style::Color};
#[cfg(test)]
use shepr_config::theme::Palette;
#[cfg(test)]
use shepr_termio::selection_render::render_selection_highlight;
#[cfg(test)]
use shepr_termio::selection_render::{
    automatic_selection_bg, automatic_selection_style, relative_luminance,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use shepr_config::PaneBordersConfig;
    use shepr_core::layout::PaneId;
    use shepr_mux::pane::PaneRuntime;
    use shepr_mux::terminal::TerminalState;
    use shepr_mux::workspace::Workspace;
    use shepr_vt::selection::Selection;

    /// A registry holding `runtime` as the live runtime of `pane_id`, keyed
    /// by the pane's terminal id the way production registers runtimes.
    fn registry_with_runtime(
        workspace: &Workspace,
        pane_id: PaneId,
        runtime: PaneRuntime,
    ) -> PaneRuntimeRegistry {
        let mut registry = PaneRuntimeRegistry::new();
        registry.insert(
            workspace
                .terminal_id(pane_id)
                .expect("test precondition")
                .clone(),
            runtime,
        );
        registry
    }

    #[test]
    fn runtimeless_pane_reserves_the_gutter_its_fresh_shell_gets() {
        let mut app = AppState::test_new();
        app.settings.pane_scrollbars = true;
        app.workspaces = vec![Workspace::test_new("resume-pending")];
        app.set_active_index(Some(0));
        let area = Rect::new(0, 0, 100, 30);

        let infos = compute_pane_infos(&app, &PaneRuntimeRegistry::new(), area);

        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].scrollbar_rect, None);
        assert_eq!(
            infos[0].inner_rect,
            shepr_mux::workspace::terminal_content_rect(
                pane_inner_rect(infos[0].rect, infos[0].borders),
                true,
                false,
            )
        );
        assert_eq!(
            infos[0].inner_rect.width,
            pane_inner_rect(infos[0].rect, infos[0].borders).width - 1
        );
    }

    #[test]
    fn unavailable_pane_renders_restore_failure_without_a_runtime() {
        let mut app = AppState::test_new();
        app.workspaces = vec![Workspace::test_new("unavailable")];
        app.set_active_index(Some(0));
        app.ensure_test_terminals();
        let pane_id = app.workspaces[0].root_pane();
        let terminal_id = app.workspaces[0]
            .terminal_id(pane_id)
            .expect("test precondition")
            .clone();
        app.terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .restore_error = Some(RestoreFailure::DirectoryUnavailable {
            path: "/missing".into(),
        });
        let runtimes = PaneRuntimeRegistry::new();
        let area = Rect::new(0, 0, 80, 24);
        let layout = crate::ui::compute_surface_for(
            &app,
            &runtimes,
            Some(app.workspaces[0].id.clone()),
            area,
        );
        let surface = crate::ui::SurfaceView {
            target: layout.target.as_ref(),
            pane_infos: &layout.pane_infos,
            split_borders: &layout.split_borders,
        };
        let cursor = crate::ui::surface_cursor(&app, &runtimes, surface);
        let backend = ratatui::backend::TestBackend::new(area.width, area.height);
        let mut terminal = ratatui::Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| crate::ui::render_surface(&app, &runtimes, surface, frame))
            .expect("render surface");
        let buffer = terminal.backend().buffer();
        let text: String = buffer
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(text.contains("Saved directory is unavailable."));
        assert!(text.contains("Restore the directory and restart this session."));
        assert!(!text.contains("Error:"));
        assert!(cursor.is_none_or(|cursor| !cursor.visible));
    }

    #[test]
    fn restore_failure_text_shows_the_error_behind_a_failed_shell() {
        let failure = RestoreFailure::shell_start_failed(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        ));
        let lines: Vec<String> = restore_failure_text(&failure)
            .lines
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("Could not start the saved shell."));
        assert_eq!(lines[1], "Error: permission denied");
    }

    #[test]
    fn pane_border_title_trims_and_truncates() {
        assert_eq!(
            pane_border_title(" claude ", 20, false).as_deref(),
            Some(" claude ")
        );
        assert_eq!(
            pane_border_title(" claude ", 20, true).as_deref(),
            Some(" claude ")
        );
        assert_eq!(pane_border_title("", 20, false), None);
        assert_eq!(
            pane_border_title("abcdef", 8, false).as_deref(),
            Some(" abc… ")
        );
        assert_eq!(
            pane_border_title("abcdef", 8, true).as_deref(),
            Some(" abc… ")
        );
        assert_eq!(pane_border_title("abcdef", 4, false), None);
    }

    #[test]
    fn pane_border_title_truncates_cjk_by_display_width() {
        let title = pane_border_title("1 模块组织（已定）", 12, false).expect("test precondition");

        assert_eq!(title, " 1 模块… ");
        assert!(display_width(title.as_str()) <= 10);
    }

    #[test]
    fn pane_border_renderer_places_adjacent_cjk_by_display_width() {
        let mut app = AppState::test_new();
        let ws = Workspace::test_new("test");
        let pane_id = ws.root_pane();
        let pane_infos = vec![PaneInfo {
            id: pane_id,
            rect: Rect::new(0, 0, 12, 3),
            inner_rect: Rect::default(),
            scrollbar_rect: None,
            borders: Borders::ALL,
            is_focused: false,
        }];

        let terminal_id = ws.panes()[&pane_id].attached_terminal_id.clone();
        let mut terminal_state = TerminalState::new(terminal_id.clone(), "/shepr-test/cwd".into());
        terminal_state.set_manual_label("1 模块组织（已定）".into());
        app.terminals.insert(terminal_id, terminal_state);

        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(12, 3))
            .expect("test precondition");
        terminal
            .draw(|frame| render_pane_borders(&app, &ws, &pane_infos, &[], frame))
            .expect("test precondition");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(4, 0)].symbol(), "模");
        assert_eq!(buffer[(5, 0)].symbol(), " ");
        assert_eq!(buffer[(6, 0)].symbol(), "块");
    }

    #[test]
    fn default_horizontal_split_uses_one_shared_divider_column() {
        let mut workspace = Workspace::test_new("test");
        let root = workspace.root_pane();
        let right = workspace.test_split(shepr_core::layout::Direction::Horizontal);
        workspace.focus_pane(root);

        let infos = apply_pane_chrome(
            &workspace
                .layout()
                .panes(shepr_core::geometry::Rect::new(0, 0, 100, 20)),
            PaneBordersConfig::Auto,
            false,
            true,
        );
        let left = infos
            .iter()
            .find(|info| info.id == root)
            .expect("test precondition");
        let right = infos
            .iter()
            .find(|info| info.id == right)
            .expect("test precondition");

        assert_eq!(left.rect.x + left.rect.width, right.rect.x);
        assert!(!left.borders.contains(Borders::RIGHT));
        assert!(right.borders.contains(Borders::LEFT));
    }

    #[test]
    fn default_vertical_split_uses_one_shared_divider_row() {
        let mut workspace = Workspace::test_new("test");
        let root = workspace.root_pane();
        let bottom = workspace.test_split(shepr_core::layout::Direction::Vertical);
        workspace.focus_pane(root);

        let infos = apply_pane_chrome(
            &workspace
                .layout()
                .panes(shepr_core::geometry::Rect::new(0, 0, 100, 20)),
            PaneBordersConfig::Auto,
            false,
            true,
        );
        let top = infos
            .iter()
            .find(|info| info.id == root)
            .expect("test precondition");
        let bottom = infos
            .iter()
            .find(|info| info.id == bottom)
            .expect("test precondition");

        assert_eq!(top.rect.y + top.rect.height, bottom.rect.y);
        assert!(!top.borders.contains(Borders::BOTTOM));
        assert!(bottom.borders.contains(Borders::TOP));
    }

    #[test]
    fn disabled_outer_borders_keep_only_shared_pane_dividers() {
        let mut workspace = Workspace::test_new("test");
        let root = workspace.root_pane();
        let right = workspace.test_split(shepr_core::layout::Direction::Horizontal);
        workspace.focus_pane(root);

        let infos = apply_pane_chrome(
            &workspace
                .layout()
                .panes(shepr_core::geometry::Rect::new(0, 0, 100, 20)),
            PaneBordersConfig::Auto,
            false,
            false,
        );
        let left = infos
            .iter()
            .find(|info| info.id == root)
            .expect("test precondition");
        let right = infos
            .iter()
            .find(|info| info.id == right)
            .expect("test precondition");

        assert_eq!(left.borders, Borders::NONE);
        assert_eq!(right.borders, Borders::LEFT);
    }

    #[test]
    fn pane_gaps_keep_independent_bordered_panes() {
        let mut workspace = Workspace::test_new("test");
        let root = workspace.root_pane();
        let right = workspace.test_split(shepr_core::layout::Direction::Horizontal);
        workspace.focus_pane(root);

        let infos = apply_pane_chrome(
            &workspace
                .layout()
                .panes(shepr_core::geometry::Rect::new(0, 0, 100, 20)),
            PaneBordersConfig::Auto,
            true,
            true,
        );
        let left = infos
            .iter()
            .find(|info| info.id == root)
            .expect("test precondition");
        let right = infos
            .iter()
            .find(|info| info.id == right)
            .expect("test precondition");

        assert_eq!(left.rect.x + left.rect.width, right.rect.x);
        assert_eq!(left.borders, Borders::ALL);
        assert_eq!(right.borders, Borders::ALL);
    }

    #[test]
    fn borderless_pane_gaps_add_one_empty_cell_between_panes() {
        let mut workspace = Workspace::test_new("test");
        let root = workspace.root_pane();
        let right = workspace.test_split(shepr_core::layout::Direction::Horizontal);
        workspace.focus_pane(root);

        let infos = apply_pane_chrome(
            &workspace
                .layout()
                .panes(shepr_core::geometry::Rect::new(0, 0, 100, 20)),
            PaneBordersConfig::Off,
            true,
            true,
        );
        let left = infos
            .iter()
            .find(|info| info.id == root)
            .expect("test precondition");
        let right = infos
            .iter()
            .find(|info| info.id == right)
            .expect("test precondition");

        assert_eq!(left.rect, Rect::new(0, 0, 49, 20));
        assert_eq!(right.rect, Rect::new(50, 0, 50, 20));
        assert!(left.borders.is_empty());
        assert!(right.borders.is_empty());
    }

    #[test]
    fn disabled_pane_borders_make_inner_rect_equal_visual_rect() {
        let mut workspace = Workspace::test_new("test");
        workspace.test_split(shepr_core::layout::Direction::Horizontal);

        let infos = apply_pane_chrome(
            &workspace
                .layout()
                .panes(shepr_core::geometry::Rect::new(0, 0, 100, 20)),
            PaneBordersConfig::Off,
            false,
            true,
        );

        for info in infos {
            assert!(info.borders.is_empty());
            assert_eq!(pane_inner_rect(info.rect, info.borders), info.rect);
        }
    }

    #[test]
    fn always_pane_borders_frame_lone_pane() {
        let workspace = Workspace::test_new("test");
        let area = shepr_core::geometry::Rect::new(0, 0, 100, 20);

        let default_infos = apply_pane_chrome(
            &workspace.layout().panes(area),
            PaneBordersConfig::Auto,
            false,
            true,
        );
        assert_eq!(default_infos[0].borders, Borders::NONE);

        let framed_infos = apply_pane_chrome(
            &workspace.layout().panes(area),
            PaneBordersConfig::Always,
            false,
            true,
        );
        assert_eq!(framed_infos[0].borders, Borders::ALL);

        let no_outer_infos = apply_pane_chrome(
            &workspace.layout().panes(area),
            PaneBordersConfig::Always,
            false,
            false,
        );
        assert_eq!(no_outer_infos[0].borders, Borders::NONE);
    }

    #[test]
    fn global_pane_border_renderer_composes_junctions_and_focus_style() {
        let mut app = AppState::test_new();
        app.settings.pane_gaps = false;
        let pane_infos = vec![
            PaneInfo {
                id: shepr_test_fixtures::fixed_pane_id(1),
                rect: Rect::new(0, 0, 2, 2),
                inner_rect: Rect::default(),
                scrollbar_rect: None,
                borders: Borders::TOP | Borders::LEFT,
                is_focused: true,
            },
            PaneInfo {
                id: shepr_test_fixtures::fixed_pane_id(2),
                rect: Rect::new(2, 0, 2, 2),
                inner_rect: Rect::default(),
                scrollbar_rect: None,
                borders: Borders::TOP | Borders::LEFT | Borders::RIGHT,
                is_focused: false,
            },
            PaneInfo {
                id: shepr_test_fixtures::fixed_pane_id(3),
                rect: Rect::new(0, 2, 2, 2),
                inner_rect: Rect::default(),
                scrollbar_rect: None,
                borders: Borders::TOP | Borders::LEFT | Borders::BOTTOM,
                is_focused: false,
            },
            PaneInfo {
                id: shepr_test_fixtures::fixed_pane_id(4),
                rect: Rect::new(2, 2, 2, 2),
                inner_rect: Rect::default(),
                scrollbar_rect: None,
                borders: Borders::ALL,
                is_focused: false,
            },
        ];
        let split_borders = vec![
            shepr_core::layout::SplitBorder {
                pos: 2,
                direction: shepr_core::layout::Direction::Horizontal,
                ratio: 0.5,
                area: shepr_core::geometry::Rect::new(0, 0, 4, 4),
                path: vec![],
            },
            shepr_core::layout::SplitBorder {
                pos: 2,
                direction: shepr_core::layout::Direction::Vertical,
                ratio: 0.5,
                area: shepr_core::geometry::Rect::new(0, 0, 4, 4),
                path: vec![shepr_core::geometry::SplitBranch::First],
            },
        ];
        let ws = Workspace::test_new("test");
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(4, 4))
            .expect("test precondition");

        terminal
            .draw(|frame| render_pane_borders(&app, &ws, &pane_infos, &split_borders, frame))
            .expect("test precondition");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(2, 2)].symbol(), "┼");
        assert_eq!(buffer[(2, 2)].style().fg, Some(app.settings.palette.accent));
        assert_eq!(buffer[(2, 1)].symbol(), "│");
        assert_eq!(buffer[(2, 1)].style().fg, Some(app.settings.palette.accent));
    }

    #[test]
    fn gapped_pane_focus_does_not_color_neighbor_border() {
        let mut app = AppState::test_new();
        app.settings.pane_gaps = true;
        let pane_infos = vec![
            PaneInfo {
                id: shepr_test_fixtures::fixed_pane_id(1),
                rect: Rect::new(0, 0, 2, 3),
                inner_rect: Rect::default(),
                scrollbar_rect: None,
                borders: Borders::ALL,
                is_focused: true,
            },
            PaneInfo {
                id: shepr_test_fixtures::fixed_pane_id(2),
                rect: Rect::new(2, 0, 2, 3),
                inner_rect: Rect::default(),
                scrollbar_rect: None,
                borders: Borders::ALL,
                is_focused: false,
            },
        ];
        let ws = Workspace::test_new("test");
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(4, 3))
            .expect("test precondition");

        terminal
            .draw(|frame| render_pane_borders(&app, &ws, &pane_infos, &[], frame))
            .expect("test precondition");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(1, 1)].style().fg, Some(app.settings.palette.accent));
        assert_eq!(
            buffer[(2, 1)].style().fg,
            Some(app.settings.palette.overlay0)
        );
    }

    #[tokio::test]
    async fn pane_scrollbar_gutter_is_reserved_before_scrollback_exists() {
        let mut app = AppState::test_new();
        let workspace = Workspace::test_new("test");
        let root_pane = workspace.root_pane();
        let terminal_runtimes = registry_with_runtime(
            &workspace,
            root_pane,
            PaneRuntime::test_with_scrollback_bytes(40, 8, 1024, b"ready\n"),
        );
        app.workspaces = vec![workspace];
        app.set_active_index(Some(0));

        let area = Rect::new(10, 3, 40, 8);
        let infos = compute_pane_infos(&app, &terminal_runtimes, area);
        let info = &infos[0];

        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, None);
        assert_eq!(info.inner_rect, Rect::new(10, 3, 39, 8));
    }

    #[tokio::test]
    async fn alternate_screen_reclaims_scrollbar_gutter_without_resizing_panes() {
        let mut app = AppState::test_new();
        let workspace = Workspace::test_new("test");
        let root_pane = workspace.root_pane();
        let terminal_id = workspace
            .terminal_id(root_pane)
            .expect("test precondition")
            .clone();
        let terminal_runtimes = registry_with_runtime(
            &workspace,
            root_pane,
            PaneRuntime::test_with_scrollback_bytes(
                40,
                8,
                1024,
                b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
            ),
        );
        let runtime = terminal_runtimes
            .get(&terminal_id)
            .expect("test precondition");
        app.workspaces = vec![workspace];
        app.set_active_index(Some(0));

        let area = Rect::new(10, 3, 40, 8);
        let assert_geometry = |expected_width, has_scrollbar| {
            let infos = compute_pane_infos(&app, &terminal_runtimes, area);
            assert_eq!(
                infos[0].inner_rect,
                Rect::new(area.x, area.y, expected_width, area.height)
            );
            assert_eq!(infos[0].scrollbar_rect.is_some(), has_scrollbar);
            assert_eq!(runtime.current_size(), (8, 40));
        };

        assert_geometry(39, true);
        runtime.test_process_pty_bytes(b"\x1b[?1049h");
        assert_geometry(40, false);
        runtime.test_process_pty_bytes(b"\x1b[?1049l");
        assert_geometry(39, true);
    }

    #[tokio::test]
    async fn lone_pane_scrollbar_gutter_is_reserved_before_scrollback_exists() {
        let mut app = AppState::test_new();
        let workspace = Workspace::test_new("test");
        let root_pane = workspace.root_pane();
        let terminal_runtimes = registry_with_runtime(
            &workspace,
            root_pane,
            PaneRuntime::test_with_scrollback_bytes(40, 8, 1024, b"ready\n"),
        );
        app.workspaces = vec![workspace];
        app.set_active_index(Some(0));

        let area = Rect::new(10, 3, 40, 8);
        let infos = compute_pane_infos(&app, &terminal_runtimes, area);
        let info = &infos[0];

        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, None);
        assert_eq!(info.inner_rect, Rect::new(10, 3, 39, 8));
    }

    #[tokio::test]
    async fn zoomed_multi_pane_keeps_border_space() {
        let mut app = AppState::test_new();
        let mut workspace = Workspace::test_new("test");
        let focused_pane = workspace.test_split(shepr_core::layout::Direction::Horizontal);
        workspace.set_zoomed(true);
        let terminal_runtimes = registry_with_runtime(
            &workspace,
            focused_pane,
            PaneRuntime::test_with_scrollback_bytes(40, 8, 1024, b"ready\n"),
        );
        app.workspaces = vec![workspace];
        app.set_active_index(Some(0));

        let area = Rect::new(10, 3, 40, 8);
        let infos = compute_pane_infos(&app, &terminal_runtimes, area);
        let info = &infos[0];

        assert_eq!(info.id, focused_pane);
        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, None);
        assert_eq!(info.inner_rect, Rect::new(11, 4, 37, 6));
    }

    #[tokio::test]
    async fn tiny_pane_does_not_reserve_scrollbar_gutter() {
        let mut app = AppState::test_new();
        let workspace = Workspace::test_new("test");
        let root_pane = workspace.root_pane();
        let terminal_runtimes = registry_with_runtime(
            &workspace,
            root_pane,
            PaneRuntime::test_with_scrollback_bytes(4, 8, 1024, b"ready\n"),
        );
        app.workspaces = vec![workspace];
        app.set_active_index(Some(0));

        let area = Rect::new(10, 3, 4, 8);
        let infos = compute_pane_infos(&app, &terminal_runtimes, area);
        let info = &infos[0];

        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, None);
        assert_eq!(info.inner_rect, area);
    }

    #[tokio::test]
    async fn rendered_content_rect_matches_the_size_new_panes_are_spawned_at() {
        for pane_borders in [
            PaneBordersConfig::Off,
            PaneBordersConfig::Auto,
            PaneBordersConfig::Always,
        ] {
            for (pane_scrollbars, zoomed) in
                [(true, false), (false, false), (true, true), (false, true)]
            {
                let mut app = AppState::test_new();
                app.settings.pane_borders = pane_borders;
                app.settings.pane_scrollbars = pane_scrollbars;
                let area = Rect::new(2, 1, 101, 31);
                let mut workspace = Workspace::test_new("test");
                let root = workspace.root_pane();
                let right = workspace.test_split(shepr_core::layout::Direction::Horizontal);
                workspace.set_zoomed(zoomed);
                let mut terminal_runtimes = PaneRuntimeRegistry::new();
                for pane in [root, right] {
                    terminal_runtimes.insert(
                        workspace
                            .terminal_id(pane)
                            .expect("test precondition")
                            .clone(),
                        PaneRuntime::test_with_scrollback_bytes(20, 5, 1024, b""),
                    );
                }
                app.workspaces = vec![workspace];
                app.set_active_index(Some(0));
                app.test_record_all_workspace_areas(area);

                let infos = compute_pane_infos(&app, &terminal_runtimes, area);
                let geometry = app.pane_geometry();
                assert_eq!(geometry.area, area);
                assert_eq!(infos.len(), if zoomed { 1 } else { 2 });
                for info in &infos {
                    assert_eq!(
                        geometry.pane_size(app.workspaces[0].layout(), zoomed, info.id),
                        Some((info.inner_rect.height, info.inner_rect.width)),
                        "borders {pane_borders:?}, scrollbars {pane_scrollbars}, zoomed {zoomed}"
                    );
                }
                for (_, runtime) in terminal_runtimes.drain() {
                    drop(runtime);
                }
            }
        }
    }

    #[tokio::test]
    async fn pane_scrollbar_setting_controls_reserved_column() {
        let mut app = AppState::test_new();
        let workspace = Workspace::test_new("test");
        let root_pane = workspace.root_pane();
        let terminal_runtimes = registry_with_runtime(
            &workspace,
            root_pane,
            PaneRuntime::test_with_scrollback_bytes(
                40,
                8,
                1024,
                b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
            ),
        );
        app.workspaces = vec![workspace];
        app.set_active_index(Some(0));

        let area = Rect::new(10, 3, 40, 8);
        let infos = compute_pane_infos(&app, &terminal_runtimes, area);
        let info = &infos[0];

        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, Some(Rect::new(49, 3, 1, 8)));
        assert_eq!(info.inner_rect, Rect::new(10, 3, 39, 8));

        app.settings.pane_scrollbars = false;
        let infos = compute_pane_infos(&app, &terminal_runtimes, area);
        let info = &infos[0];

        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, None);
        assert_eq!(info.inner_rect, area);
    }

    /// Scroll metrics for a live view with no scrollback: viewport row N is
    /// absolute row N.
    fn zero_origin_metrics(viewport_rows: usize) -> Option<shepr_termio::ScrollMetrics> {
        Some(shepr_termio::ScrollMetrics {
            offset_from_bottom: 0,
            max_offset_from_bottom: 0,
            viewport_rows,
            history_origin: shepr_vt::AbsRow(0),
        })
    }

    /// A selection-highlight sink that applies each style to `buffer`, skipping positions
    /// the buffer does not have.
    fn sink(buffer: &mut Buffer) -> impl FnMut(u16, u16, Style) + '_ {
        |x, y, style| {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_style(style);
            }
        }
    }

    #[test]
    fn selection_highlight_uses_one_uniform_style() {
        let palette = Palette::catppuccin();
        let host_theme = shepr_termio::host_term::theme::TerminalTheme {
            foreground: None,
            background: Some(shepr_termio::host_term::theme::RgbColor {
                r: 12,
                g: 14,
                b: 16,
            }),
            ..Default::default()
        };
        let expected_style = automatic_selection_style(&palette, host_theme);
        let selection = Some(Selection::range(
            shepr_test_fixtures::fixed_pane_id(1),
            shepr_vt::Point::new(shepr_vt::AbsRow(0), 0),
            shepr_vt::Point::new(shepr_vt::AbsRow(0), 2),
        ));
        let backend = ratatui::backend::TestBackend::new(4, 1);
        let mut terminal = ratatui::Terminal::new(backend).expect("test precondition");

        terminal
            .draw(|frame| {
                let buf = frame.buffer_mut();
                buf[(0, 0)].set_style(
                    Style::default()
                        .fg(Color::Rgb(10, 220, 120))
                        .bg(Color::Black),
                );
                buf[(1, 0)].set_style(
                    Style::default()
                        .fg(Color::Rgb(220, 180, 40))
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                );
                buf[(2, 0)].set_style(Style::default().fg(Color::Blue).bg(Color::Reset));
                render_selection_highlight(
                    selection.as_ref(),
                    &shepr_test_fixtures::fixed_pane_id(1),
                    Rect::new(0, 0, 4, 1),
                    zero_origin_metrics(1),
                    &palette,
                    host_theme,
                    &mut |x, y, style| {
                        if let Some(cell) = buf.cell_mut((x, y)) {
                            cell.set_style(style);
                        }
                    },
                );
            })
            .expect("test precondition");

        let buffer = terminal.backend().buffer();
        let first = buffer[(0, 0)].style();
        let second = buffer[(1, 0)].style();
        let third = buffer[(2, 0)].style();

        assert_eq!(first.fg, expected_style.fg);
        assert_eq!(second.fg, expected_style.fg);
        assert_eq!(third.fg, expected_style.fg);
        assert_eq!(first.bg, expected_style.bg);
        assert_eq!(second.bg, expected_style.bg);
        assert_eq!(third.bg, expected_style.bg);
        assert_eq!(first.add_modifier, expected_style.add_modifier);
        assert_eq!(second.add_modifier, expected_style.add_modifier);
        assert_eq!(third.add_modifier, expected_style.add_modifier);
        assert!(!second.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn selection_highlight_clips_pane_rect_larger_than_buffer() {
        // The client can compose a pane surface produced for another layout, so the
        // pane's inner rect may reach past the frame. Painting must clip, not panic.
        let palette = Palette::catppuccin();
        let host_theme = shepr_termio::host_term::theme::TerminalTheme::default();
        let expected = automatic_selection_style(&palette, host_theme);
        let selection = Some(Selection::range(
            shepr_test_fixtures::fixed_pane_id(1),
            shepr_vt::Point::new(shepr_vt::AbsRow(0), 0),
            shepr_vt::Point::new(shepr_vt::AbsRow(2), 3),
        ));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 2));

        render_selection_highlight(
            selection.as_ref(),
            &shepr_test_fixtures::fixed_pane_id(1),
            Rect::new(1, 1, 4, 3),
            zero_origin_metrics(3),
            &palette,
            host_theme,
            &mut sink(&mut buffer),
        );

        // Pane-relative (0, 0)..(0, 2) lands on screen row 1, columns 1..=3; the
        // rest of the pane is off-buffer and silently skipped.
        for x in 1..4 {
            assert_eq!(buffer[(x, 1)].style().bg, expected.bg, "column {x}");
        }
        assert_ne!(buffer[(0, 1)].style().bg, expected.bg);
        for x in 0..4 {
            assert_ne!(buffer[(x, 0)].style().bg, expected.bg, "row 0 column {x}");
        }

        // A rect entirely outside the buffer paints nothing and does not panic.
        render_selection_highlight(
            selection.as_ref(),
            &shepr_test_fixtures::fixed_pane_id(1),
            Rect::new(10, 10, 4, 3),
            zero_origin_metrics(3),
            &palette,
            host_theme,
            &mut sink(&mut buffer),
        );

        // Without scroll metrics viewport rows cannot be mapped to the
        // selection's absolute rows, so nothing is painted.
        let mut unmapped = Buffer::empty(Rect::new(0, 0, 4, 2));
        render_selection_highlight(
            selection.as_ref(),
            &shepr_test_fixtures::fixed_pane_id(1),
            Rect::new(0, 0, 4, 2),
            None,
            &palette,
            host_theme,
            &mut sink(&mut unmapped),
        );
        assert_eq!(unmapped, Buffer::empty(Rect::new(0, 0, 4, 2)));
    }

    #[test]
    fn automatic_selection_background_uses_host_background() {
        let bg = automatic_selection_bg(
            &Palette::terminal(),
            shepr_termio::host_term::theme::TerminalTheme {
                foreground: Some(shepr_termio::host_term::theme::RgbColor {
                    r: 230,
                    g: 230,
                    b: 230,
                }),
                background: Some(shepr_termio::host_term::theme::RgbColor {
                    r: 12,
                    g: 14,
                    b: 16,
                }),
                ..Default::default()
            },
        );

        let Color::Rgb(r, g, b) = bg else {
            panic!("selection background should resolve to rgb");
        };
        assert!(relative_luminance((r, g, b)) > relative_luminance((12, 14, 16)));
    }

    #[test]
    fn automatic_selection_rgb_style_is_readable_with_or_without_host_background() {
        for (background, selected_bg, selected_fg) in [
            ((239, 241, 245), (172, 174, 176), (0, 0, 0)),
            ((26, 27, 38), (90, 91, 99), (255, 255, 255)),
            ((45, 53, 59), (104, 110, 114), (255, 255, 255)),
        ] {
            let mut palette = Palette::catppuccin();
            let (r, g, b) = background;
            palette.panel_bg = Color::Rgb(r, g, b);
            let expected = Style::reset()
                .bg(Color::Rgb(selected_bg.0, selected_bg.1, selected_bg.2))
                .fg(Color::Rgb(selected_fg.0, selected_fg.1, selected_fg.2));

            assert_eq!(
                automatic_selection_style(&palette, Default::default()),
                expected
            );
            assert_eq!(
                automatic_selection_style(
                    &Palette::terminal(),
                    shepr_termio::host_term::theme::TerminalTheme {
                        background: Some(shepr_termio::host_term::theme::RgbColor { r, g, b }),
                        ..Default::default()
                    },
                ),
                expected
            );
        }
    }

    #[test]
    fn automatic_selection_preserves_symbolic_palette_fallbacks() {
        let mut palette = Palette::terminal();
        assert_eq!(
            automatic_selection_style(&palette, Default::default()),
            Style::reset().fg(Color::White).bg(Color::DarkGray)
        );
        for fallback in [Color::Blue, Color::White, Color::Indexed(42), Color::Reset] {
            palette.surface_dim = fallback;
            assert_eq!(
                automatic_selection_bg(&palette, Default::default()),
                fallback
            );
        }
    }
}
