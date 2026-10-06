use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Paragraph, Widget, Wrap},
};

use super::PaneSurface;
use super::chrome::{overlay_buffer, put_run_with};
use super::scrollbar::render_pane_scrollbar;
use super::text::truncate_end;
use crate::app::AppState;
use shepr_mux::pane::{PaneRuntime, PaneRuntimeRegistry};
use shepr_mux::terminal::{Label, PaneStartFailure};
use shepr_protocol::{CellData, ChromeRole, FrameData, WireColor};

// Two pane-edge cells and the title's two padding cells leave one column for
// the shortest nonempty label.
// limits-exempt: the border title's drawn layout, edge and padding cells.
const PANE_BORDER_TITLE_OVERHEAD_COLS: u16 = 4;
const MIN_PANE_WIDTH_FOR_BORDER_TITLE: u16 = PANE_BORDER_TITLE_OVERHEAD_COLS + 1;

pub(super) fn pane_is_scrolled_back(rt: &PaneRuntime) -> bool {
    rt.read()
        .scroll_metrics()
        .is_some_and(|metrics| metrics.offset_from_bottom > 0)
}

/// The unavailable-pane text: the guidance, then the error behind it on its
/// own line. OS errors are formatted only at this presentation boundary.
fn restore_failure_text(failure: &PaneStartFailure) -> Text<'_> {
    let mut lines = vec![Line::raw(failure.guidance())];
    if let Some(cause) = failure.cause() {
        lines.push(Line::from(vec![Span::raw("Error: "), Span::raw(cause)]));
    }
    Text::from(lines)
}

fn pane_border_title(label: &Label, pane_width: u16) -> Option<String> {
    if pane_width < MIN_PANE_WIDTH_FOR_BORDER_TITLE {
        return None;
    }
    let max_label_width = pane_width.saturating_sub(PANE_BORDER_TITLE_OVERHEAD_COLS) as usize;
    Some(format!(
        " {} ",
        truncate_end(label.as_str(), max_label_width)
    ))
}

/// The chrome of the panes `workspace` shows in `area`: its whole layout, or
/// the zoomed pane alone.
pub(super) fn visible_chromes(
    app: &AppState,
    workspace: &shepr_mux::workspace::Workspace,
    area: shepr_core::geometry::Rect,
) -> Vec<shepr_core::chrome::PaneChrome> {
    app.chrome_in(area)
        .visible_panes(workspace.tree().layout(), workspace.tree().zoomed())
}

/// The live runtime of `pane` in `workspace`: the workspace's own tree confirms
/// the pane is one of its own in one probe, so the draw never walks the set.
pub(crate) fn workspace_runtime<'a>(
    workspace: &shepr_mux::workspace::Workspace,
    terminal_runtimes: &'a PaneRuntimeRegistry,
    pane: shepr_core::layout::PaneId,
) -> Option<&'a PaneRuntime> {
    workspace.tree().pane(pane)?;
    terminal_runtimes.get(&pane)
}

/// Draws the panes of one workspace and their chrome into `frame`: pane cells
/// go straight to the wire form, the chrome over them afterwards.
pub(super) fn render_panes(
    app: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
    frame: &mut FrameData,
    target: Option<super::SurfaceTarget>,
    pane_infos: &[PaneSurface],
    split_borders: &[shepr_core::layout::SplitBorder],
) -> Vec<(shepr_core::layout::PaneId, shepr_mux::pane::PaneDraw)> {
    let mut draws = Vec::new();
    let Some(ws) = target.and_then(|target| target.workspace(app)) else {
        return draws;
    };

    for info in pane_infos {
        if let Some(rt) = workspace_runtime(ws, terminal_runtimes, info.id) {
            draws.push((info.id, rt.read().render_into(frame, info.inner_rect)));
            render_pane_scrollbar(frame, info, rt);
        } else if let Some(reason) = ws
            .tree()
            .pane(info.id)
            .and_then(|record| record.terminal().start_failure())
        {
            let mut scratch = Buffer::empty(info.inner_rect);
            Paragraph::new(restore_failure_text(reason))
                .wrap(Wrap { trim: false })
                .render(info.inner_rect, &mut scratch);
            // A pane with no runtime has no cells of its own: the message owns
            // its whole content rect.
            overlay_buffer(frame, &scratch, info.inner_rect);
        }
    }

    render_pane_borders(app, ws, pane_infos, split_borders, frame);
    draws
}

#[derive(Clone, Copy, Default)]
struct LineCell(u8);

impl LineCell {
    // limits-exempt: each value is a bit position in the border cell's flag byte.
    const PRESENT: u8 = 1 << 0;
    // limits-exempt: each value is a bit position in the border cell's flag byte.
    const UP: u8 = 1 << 1;
    // limits-exempt: each value is a bit position in the border cell's flag byte.
    const DOWN: u8 = 1 << 2;
    // limits-exempt: each value is a bit position in the border cell's flag byte.
    const LEFT: u8 = 1 << 3;
    // limits-exempt: each value is a bit position in the border cell's flag byte.
    const RIGHT: u8 = 1 << 4;
    // limits-exempt: each value is a bit position in the border cell's flag byte.
    const FOCUSED: u8 = 1 << 5;

    fn has_any(self, flags: u8) -> bool {
        self.0 & flags != 0
    }

    fn set(&mut self, flag: u8) {
        self.0 |= flag;
    }

    fn set_if(&mut self, flag: u8, condition: bool) {
        if condition {
            self.set(flag);
        }
    }
}

struct BorderGrid {
    width: u16,
    height: u16,
    cells: Vec<LineCell>,
}

impl BorderGrid {
    fn new(frame: &FrameData) -> Self {
        Self {
            width: frame.width(),
            height: frame.height(),
            cells: vec![LineCell::default(); frame.cells().len()],
        }
    }

    fn index(&self, x: u16, y: u16) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = usize::from(y)
            .checked_mul(usize::from(self.width))?
            .checked_add(usize::from(x))?;
        (index < self.cells.len()).then_some(index)
    }

    fn contains(&self, x: u16, y: u16) -> bool {
        self.get(x, y)
            .is_some_and(|cell| cell.has_any(LineCell::PRESENT))
    }

    fn get(&self, x: u16, y: u16) -> Option<LineCell> {
        self.index(x, y).map(|index| self.cells[index])
    }

    fn entry(&mut self, x: u16, y: u16) -> Option<&mut LineCell> {
        let index = self.index(x, y)?;
        let cell = &mut self.cells[index];
        cell.set(LineCell::PRESENT);
        Some(cell)
    }

    fn mark_focused(&mut self, x: u16, y: u16) {
        if let Some(index) = self.index(x, y) {
            let cell = &mut self.cells[index];
            if cell.has_any(LineCell::PRESENT) {
                cell.set(LineCell::FOCUSED);
            }
        }
    }
}

fn render_pane_borders(
    app: &AppState,
    ws: &shepr_mux::workspace::Workspace,
    pane_infos: &[PaneSurface],
    split_borders: &[shepr_core::layout::SplitBorder],
    frame: &mut FrameData,
) {
    let mut cells = BorderGrid::new(frame);
    for info in pane_infos {
        add_pane_border_cells(&mut cells, info);
    }
    add_split_border_cells(app.settings().pane_gaps, split_borders, &mut cells);
    for info in pane_infos.iter().filter(|info| info.is_focused) {
        mark_focused_pane_cells(&mut cells, info, app.settings().pane_gaps);
    }

    // Each row's border strokes are written as runs of adjacent cells, each
    // stroke written straight into its frame cell. The glyph repair scans the
    // whole row once per run, so a cell-at-a-time write would rescan it for
    // every cell of a horizontal border. Strokes are one column wide, so a run
    // repairs exactly what writing its cells one by one would.
    let blank = CellData::blank();
    for y in 0..cells.height {
        let row_start = usize::from(y) * usize::from(cells.width);
        let row = &cells.cells[row_start..row_start + usize::from(cells.width)];
        let stroke = |column: u16| {
            let line = row[usize::from(column)];
            line.has_any(LineCell::PRESENT) && !line_cell_symbol(line).is_empty()
        };
        let mut x = 0;
        while x < cells.width {
            if !stroke(x) {
                x += 1;
                continue;
            }
            let start = x;
            while x < cells.width && stroke(x) {
                x += 1;
            }
            put_run_with(frame, start, y, usize::from(x - start), |index, slot| {
                let line = row[usize::from(start) + index];
                slot.clone_from(&blank);
                slot.symbol.clear();
                slot.symbol.push_str(line_cell_symbol(line));
                slot.fg = WireColor::Chrome(if line.has_any(LineCell::FOCUSED) {
                    ChromeRole::BorderFocused
                } else {
                    ChromeRole::Border
                });
            });
        }
    }

    render_pane_border_titles(ws, pane_infos, frame);
}

fn add_split_border_cells(
    pane_gaps: bool,
    split_borders: &[shepr_core::layout::SplitBorder],
    cells: &mut BorderGrid,
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
                    if !cells.contains(x, y) {
                        continue;
                    }
                    let left = x
                        .checked_sub(1)
                        .and_then(|left_x| cells.get(left_x, y))
                        .is_some_and(|cell| cell.has_any(LineCell::LEFT | LineCell::RIGHT));
                    let right = cells
                        .get(x.saturating_add(1), y)
                        .is_some_and(|cell| cell.has_any(LineCell::LEFT | LineCell::RIGHT));
                    let Some(cell) = cells.entry(x, y) else {
                        continue;
                    };
                    cell.set_if(LineCell::UP, y > split.area.y);
                    cell.set_if(LineCell::DOWN, y + 1 < end);
                    cell.set_if(LineCell::LEFT, left);
                    cell.set_if(LineCell::RIGHT, right);
                }
            }
            shepr_core::layout::Direction::Vertical => {
                let y = split.pos;
                let end = split.area.x.saturating_add(split.area.width);
                for x in split.area.x..=end {
                    if !cells.contains(x, y) {
                        continue;
                    }
                    let up = y
                        .checked_sub(1)
                        .and_then(|up_y| cells.get(x, up_y))
                        .is_some_and(|cell| cell.has_any(LineCell::UP | LineCell::DOWN));
                    let down = cells
                        .get(x, y.saturating_add(1))
                        .is_some_and(|cell| cell.has_any(LineCell::UP | LineCell::DOWN));
                    let Some(cell) = cells.entry(x, y) else {
                        continue;
                    };
                    cell.set_if(LineCell::LEFT, x > split.area.x);
                    cell.set_if(LineCell::RIGHT, x + 1 < end);
                    cell.set_if(LineCell::UP, up);
                    cell.set_if(LineCell::DOWN, down);
                }
            }
        }
    }
}

fn add_pane_border_cells(cells: &mut BorderGrid, info: &PaneSurface) {
    let rect = info.rect;
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let right = rect.x.saturating_add(rect.width).saturating_sub(1);
    let bottom = rect.y.saturating_add(rect.height).saturating_sub(1);

    for x in rect.x..=right {
        let Some(cell) = cells.entry(x, rect.y) else {
            continue;
        };
        cell.set_if(LineCell::LEFT, x > rect.x);
        cell.set_if(LineCell::RIGHT, x < right);
    }
    if !info.shared_edges.shares_bottom {
        for x in rect.x..=right {
            let Some(cell) = cells.entry(x, bottom) else {
                continue;
            };
            cell.set_if(LineCell::LEFT, x > rect.x);
            cell.set_if(LineCell::RIGHT, x < right);
        }
    }
    for y in rect.y..=bottom {
        let Some(cell) = cells.entry(rect.x, y) else {
            continue;
        };
        cell.set_if(LineCell::UP, y > rect.y);
        cell.set_if(LineCell::DOWN, y < bottom);
    }
    if !info.shared_edges.shares_right {
        for y in rect.y..=bottom {
            let Some(cell) = cells.entry(right, y) else {
                continue;
            };
            cell.set_if(LineCell::UP, y > rect.y);
            cell.set_if(LineCell::DOWN, y < bottom);
        }
    }
}

fn mark_focused_pane_cells(cells: &mut BorderGrid, info: &PaneSurface, pane_gaps: bool) {
    let rect = info.rect;
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let right = rect.x.saturating_add(rect.width).saturating_sub(1);
    let bottom = rect.y.saturating_add(rect.height).saturating_sub(1);
    let shared_right = rect.x.saturating_add(rect.width);
    let shared_bottom = rect.y.saturating_add(rect.height);
    for y in rect.y..=bottom {
        cells.mark_focused(rect.x, y);
        cells.mark_focused(right, y);
        if !pane_gaps {
            cells.mark_focused(shared_right, y);
        }
    }
    for x in rect.x..=right {
        cells.mark_focused(x, rect.y);
        cells.mark_focused(x, bottom);
        if !pane_gaps {
            cells.mark_focused(x, shared_bottom);
        }
    }
    if !pane_gaps {
        cells.mark_focused(shared_right, shared_bottom);
    }
}

fn render_pane_border_titles(
    ws: &shepr_mux::workspace::Workspace,
    pane_infos: &[PaneSurface],
    frame: &mut FrameData,
) {
    let area = Rect::new(0, 0, frame.width(), frame.height());
    for info in pane_infos {
        let Some(title) = ws
            .tree()
            .pane(info.id)
            .and_then(|record| record.terminal().manual_label_value())
            .and_then(|label| pane_border_title(label, info.rect.width))
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
        let mut style = Style::default();
        if info.is_focused {
            style = style.add_modifier(Modifier::BOLD);
        }
        let width = end_x.saturating_sub(start_x);
        let mut scratch = Buffer::empty(Rect::new(start_x, y, width, 1));
        // The title owns what it wrote, its padding spaces included; the
        // border stroke after it stays.
        let (end, _) = scratch.set_stringn(start_x, y, title, usize::from(width), style);
        let written = Rect::new(start_x, y, end.saturating_sub(start_x), 1);
        overlay_buffer(frame, &scratch, written);
        // A ratatui colour cannot name a chrome role, so the title takes its
        // role once it is in the frame.
        let role = WireColor::Chrome(if info.is_focused {
            ChromeRole::BorderFocused
        } else {
            ChromeRole::Border
        });
        let row_start = usize::from(y) * usize::from(frame.width());
        let span = usize::from(written.x)..usize::from(written.right().min(frame.width()));
        for cell in &mut frame.cells_mut()[row_start..][span] {
            cell.fg = role;
        }
    }
}

fn line_cell_symbol(line: LineCell) -> &'static str {
    match (
        line.has_any(LineCell::UP),
        line.has_any(LineCell::DOWN),
        line.has_any(LineCell::LEFT),
        line.has_any(LineCell::RIGHT),
    ) {
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

/// The cell strip a split divider can be grabbed on: the divider line itself
/// when neighbouring panes share one, or that line and the border of the pane
/// before it when `pane_gaps` keeps each pane's own box.
/// The cell strip a split divider can be grabbed on: the divider line itself
/// when neighbouring panes share one, or that line and the border of the pane
/// before it when `pane_gaps` keeps each pane's own box.
pub(crate) fn split_hit_rect(split: &shepr_core::layout::SplitBorder, pane_gaps: bool) -> Rect {
    match (split.direction, pane_gaps) {
        (shepr_core::layout::Direction::Horizontal, false) => {
            Rect::new(split.pos, split.area.y, 1, split.area.height)
        }
        (shepr_core::layout::Direction::Horizontal, true) => {
            let start = split.pos.saturating_sub(1);
            Rect::new(
                start,
                split.area.y,
                split.pos.saturating_sub(start).saturating_add(1),
                split.area.height,
            )
        }
        (shepr_core::layout::Direction::Vertical, false) => {
            Rect::new(split.area.x, split.pos, split.area.width, 1)
        }
        (shepr_core::layout::Direction::Vertical, true) => {
            let start = split.pos.saturating_sub(1);
            Rect::new(
                split.area.x,
                start,
                split.area.width,
                split.pos.saturating_sub(start).saturating_add(1),
            )
        }
    }
}

#[cfg(test)]
use super::text::display_width;

#[cfg(test)]
fn compute_pane_infos(
    app: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
    area: Rect,
) -> Vec<PaneSurface> {
    let Some(workspace) = app
        .bookmark_index()
        .and_then(|index| app.workspaces().as_slice().get(index))
    else {
        return Vec::new();
    };
    super::compute_pane_surfaces(app, terminal_runtimes, workspace, area)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use shepr_core::chrome::SharedPaneEdges;
    use shepr_core::layout::PaneId;
    use shepr_mux::pane::PaneRuntime;
    use shepr_mux::workspace::Workspace;

    /// A registry holding `runtime` as the live runtime of `pane_id`, keyed
    /// by the pane the way production registers runtimes.
    fn registry_with_runtime(
        workspace: &Workspace,
        pane_id: PaneId,
        runtime: PaneRuntime,
    ) -> PaneRuntimeRegistry {
        assert!(
            workspace.tree().pane(pane_id).is_some(),
            "test precondition"
        );
        let mut registry = PaneRuntimeRegistry::default();
        registry.insert(pane_id, runtime);
        registry
    }

    #[test]
    fn runtimeless_pane_reserves_the_gutter_its_fresh_shell_gets() {
        let mut app = AppState::test_new();
        app.settings_mut().pane_scrollbars = true;
        app.test_set_workspaces(vec![Workspace::test_new("resume-pending")]);
        app.seed_bookmark_index(Some(0));
        let area = Rect::new(0, 0, 100, 30);

        let infos = compute_pane_infos(&app, &PaneRuntimeRegistry::default(), area);

        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].scrollbar_rect, None);
        // A lone pane is framed on every side; the gutter is the one column
        // inside the frame its fresh shell does not get.
        assert_eq!(infos[0].shared_edges, SharedPaneEdges::default());
        assert_eq!(
            infos[0].inner_rect,
            Rect::new(
                infos[0].rect.x + 1,
                infos[0].rect.y + 1,
                infos[0].rect.width - 3,
                infos[0].rect.height - 2
            )
        );
    }

    #[test]
    fn unavailable_pane_renders_restore_failure_without_a_runtime() {
        let mut app = AppState::test_new();
        app.test_set_workspaces(vec![Workspace::test_new("unavailable")]);
        app.seed_bookmark_index(Some(0));
        let pane_id = app.ws(0).tree().root();
        app.terminal_mut(pane_id)
            .record_start_failure(PaneStartFailure::DirectoryUnavailable {
                path: "/missing".into(),
                error: std::io::Error::from(std::io::ErrorKind::NotFound),
            });
        let runtimes = PaneRuntimeRegistry::default();
        // Wide enough that the framed pane's content does not wrap the
        // message mid-sentence.
        let area = Rect::new(0, 0, 100, 24);
        let target = crate::ui::SurfaceTarget {
            index: 0,
            id: app.ws(0).id(),
        };
        let layout = crate::ui::compute_surface_for(&app, &runtimes, Some(target), area);
        let surface = crate::ui::SurfaceView {
            target: layout.target,
            panes: &layout.panes,
            split_borders: &layout.split_borders,
        };
        let cursor = crate::ui::surface_cursor(&app, &runtimes, surface);
        let mut frame =
            FrameData::blank(area.width, area.height).expect("test frame size is valid");
        crate::ui::render_surface(&app, &runtimes, surface, &mut frame);
        let text: String = frame
            .cells()
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect();
        assert!(text.contains("Pane directory is unavailable."));
        assert!(text.contains("Restore the directory and restart this session."));
        assert!(text.contains("Error:"));
        assert!(cursor.is_none_or(|cursor| !cursor.visible));
    }

    #[test]
    fn restore_failure_text_shows_the_error_behind_a_failed_shell() {
        let failure = PaneStartFailure::shell_start_failed(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        ));
        let lines: Vec<String> = restore_failure_text(&failure)
            .lines
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("Could not start the pane shell."));
        assert_eq!(lines[1], "Error: permission denied");
    }

    #[test]
    fn pane_border_title_uses_labels_and_truncates() {
        assert_eq!(
            pane_border_title(&Label::new(" claude ").expect("label"), 20).as_deref(),
            Some(" claude ")
        );
        assert_eq!(Label::new(" "), None);
        assert_eq!(
            pane_border_title(&Label::new("abcdef").expect("label"), 8).as_deref(),
            Some(" abc… ")
        );
        assert_eq!(
            pane_border_title(&Label::new("abcdef").expect("label"), 4),
            None
        );
    }

    #[test]
    fn pane_border_title_truncates_cjk_by_display_width() {
        let label = Label::new("1 模块组织（已定）").expect("test label");
        let title = pane_border_title(&label, 12).expect("test precondition");

        assert_eq!(title, " 1 模块… ");
        assert!(display_width(title.as_str()) <= 10);
    }

    #[test]
    fn pane_border_renderer_places_adjacent_cjk_by_display_width() {
        let app = AppState::test_new();
        let mut ws = Workspace::test_new("test");
        let pane_id = ws.tree().root();
        let pane_infos = vec![PaneSurface {
            id: pane_id,
            rect: Rect::new(0, 0, 12, 3),
            inner_rect: Rect::default(),
            scrollbar_gutter: None,
            scrollbar_rect: None,
            shared_edges: SharedPaneEdges::default(),
            is_focused: false,
        }];

        ws.pane_mut(pane_id)
            .expect("the root pane has a record")
            .terminal_mut()
            .set_manual_label(
                shepr_mux::terminal::Label::new("1 模块组织（已定）").expect("test label"),
            );

        let mut frame = FrameData::blank(12, 3).expect("test frame size is valid");
        render_pane_borders(&app, &ws, &pane_infos, &[], &mut frame);
        let buffer = |x: u16, y: u16| &frame.cells()[usize::from(y) * 12 + usize::from(x)];
        assert_eq!(buffer(4, 0).symbol, "模");
        assert_eq!(buffer(5, 0).symbol, " ");
        assert_eq!(buffer(6, 0).symbol, "块");
    }

    /// A title is laid out through ratatui, which has no chrome roles, so it
    /// takes its border's role once it is in the frame.
    #[test]
    fn a_pane_title_takes_its_border_role() {
        let mut ws = Workspace::test_new("test");
        let pane = ws.tree().root();
        ws.pane_mut(pane)
            .expect("root pane")
            .terminal_mut()
            .set_manual_label(shepr_mux::terminal::Label::new("build").expect("test label"));
        let surface = |is_focused| PaneSurface {
            id: pane,
            rect: Rect::new(0, 0, 12, 3),
            inner_rect: Rect::new(1, 1, 10, 1),
            scrollbar_gutter: None,
            scrollbar_rect: None,
            shared_edges: SharedPaneEdges::default(),
            is_focused,
        };
        for (is_focused, role) in [
            (true, ChromeRole::BorderFocused),
            (false, ChromeRole::Border),
        ] {
            let mut frame = FrameData::blank(12, 3).expect("test frame size is valid");
            render_pane_border_titles(&ws, &[surface(is_focused)], &mut frame);
            let top: String = frame.cells()[..12]
                .iter()
                .map(|cell| cell.symbol.as_str())
                .collect();
            let start = top.find("build").expect("the title is drawn");
            for cell in &frame.cells()[start..start + "build".len()] {
                assert_eq!(cell.fg, WireColor::Chrome(role), "{top:?}");
            }
        }
    }

    #[test]
    fn global_pane_border_renderer_composes_junctions_and_focus_style() {
        let mut app = AppState::test_new();
        app.settings_mut().pane_gaps = false;
        let pane_infos = vec![
            PaneSurface {
                id: shepr_test_fixtures::fixed_pane_id(1),
                rect: Rect::new(0, 0, 2, 2),
                inner_rect: Rect::default(),
                scrollbar_gutter: None,
                scrollbar_rect: None,
                shared_edges: SharedPaneEdges {
                    shares_right: true,
                    shares_bottom: true,
                },
                is_focused: true,
            },
            PaneSurface {
                id: shepr_test_fixtures::fixed_pane_id(2),
                rect: Rect::new(2, 0, 2, 2),
                inner_rect: Rect::default(),
                scrollbar_gutter: None,
                scrollbar_rect: None,
                shared_edges: SharedPaneEdges {
                    shares_right: false,
                    shares_bottom: true,
                },
                is_focused: false,
            },
            PaneSurface {
                id: shepr_test_fixtures::fixed_pane_id(3),
                rect: Rect::new(0, 2, 2, 2),
                inner_rect: Rect::default(),
                scrollbar_gutter: None,
                scrollbar_rect: None,
                shared_edges: SharedPaneEdges {
                    shares_right: true,
                    shares_bottom: false,
                },
                is_focused: false,
            },
            PaneSurface {
                id: shepr_test_fixtures::fixed_pane_id(4),
                rect: Rect::new(2, 2, 2, 2),
                inner_rect: Rect::default(),
                scrollbar_gutter: None,
                scrollbar_rect: None,
                shared_edges: SharedPaneEdges::default(),
                is_focused: false,
            },
        ];
        let split_borders = vec![
            shepr_core::layout::SplitBorder {
                pos: 2,
                direction: shepr_core::layout::Direction::Horizontal,
                ratio: shepr_core::layout::SplitRatio::EVEN,
                area: shepr_core::geometry::Rect::new(0, 0, 4, 4),
                path: shepr_core::layout::SplitPath::default(),
            },
            shepr_core::layout::SplitBorder {
                pos: 2,
                direction: shepr_core::layout::Direction::Vertical,
                ratio: shepr_core::layout::SplitRatio::EVEN,
                area: shepr_core::geometry::Rect::new(0, 0, 4, 4),
                path: vec![shepr_core::layout::SplitBranch::First].into(),
            },
        ];
        let ws = Workspace::test_new("test");
        let mut frame = FrameData::blank(4, 4).expect("test frame size is valid");
        render_pane_borders(&app, &ws, &pane_infos, &split_borders, &mut frame);
        let buffer = |x: u16, y: u16| &frame.cells()[usize::from(y) * 4 + usize::from(x)];
        let accent = WireColor::Chrome(ChromeRole::BorderFocused);
        assert_eq!(buffer(2, 2).symbol, "┼");
        assert_eq!(buffer(2, 2).fg, accent);
        assert_eq!(buffer(2, 1).symbol, "│");
        assert_eq!(buffer(2, 1).fg, accent);
    }

    #[test]
    fn gapped_pane_focus_does_not_color_neighbor_border() {
        let mut app = AppState::test_new();
        app.settings_mut().pane_gaps = true;
        let pane_infos = vec![
            PaneSurface {
                id: shepr_test_fixtures::fixed_pane_id(1),
                rect: Rect::new(0, 0, 2, 3),
                inner_rect: Rect::default(),
                scrollbar_gutter: None,
                scrollbar_rect: None,
                shared_edges: SharedPaneEdges::default(),
                is_focused: true,
            },
            PaneSurface {
                id: shepr_test_fixtures::fixed_pane_id(2),
                rect: Rect::new(2, 0, 2, 3),
                inner_rect: Rect::default(),
                scrollbar_gutter: None,
                scrollbar_rect: None,
                shared_edges: SharedPaneEdges::default(),
                is_focused: false,
            },
        ];
        let ws = Workspace::test_new("test");
        let mut frame = FrameData::blank(4, 3).expect("test frame size is valid");
        render_pane_borders(&app, &ws, &pane_infos, &[], &mut frame);
        let buffer = |x: u16, y: u16| &frame.cells()[usize::from(y) * 4 + usize::from(x)];
        assert_eq!(
            buffer(1, 1).fg,
            WireColor::Chrome(ChromeRole::BorderFocused)
        );
        assert_eq!(buffer(2, 1).fg, WireColor::Chrome(ChromeRole::Border));
    }

    #[tokio::test]
    async fn pane_scrollbar_gutter_is_reserved_before_scrollback_exists() {
        let mut app = AppState::test_new();
        let workspace = Workspace::test_new("test");
        let root_pane = workspace.tree().root();
        let terminal_runtimes = registry_with_runtime(
            &workspace,
            root_pane,
            PaneRuntime::test_with_scrollback_bytes(40, 8, 1024, b"ready\n"),
        );
        app.test_set_workspaces(vec![workspace]);
        app.seed_bookmark_index(Some(0));

        let area = Rect::new(10, 3, 40, 8);
        let infos = compute_pane_infos(&app, &terminal_runtimes, area);
        let info = &infos[0];

        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, None);
        assert_eq!(info.inner_rect, Rect::new(11, 4, 37, 6));
    }

    #[tokio::test]
    async fn alternate_screen_reclaims_scrollbar_gutter_without_resizing_panes() {
        let mut app = AppState::test_new();
        let workspace = Workspace::test_new("test");
        let root_pane = workspace.tree().root();
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
            .get(&root_pane)
            .expect("test precondition");
        app.test_set_workspaces(vec![workspace]);
        app.seed_bookmark_index(Some(0));

        let area = Rect::new(10, 3, 40, 8);
        let assert_geometry = |expected_width, has_scrollbar| {
            let infos = compute_pane_infos(&app, &terminal_runtimes, area);
            assert_eq!(
                infos[0].inner_rect,
                Rect::new(area.x + 1, area.y + 1, expected_width, area.height - 2)
            );
            assert_eq!(infos[0].scrollbar_rect.is_some(), has_scrollbar);
            assert_eq!(runtime.current_size(), (8, 40));
        };

        assert_geometry(37, true);
        runtime.test_process_pty_bytes(b"\x1b[?1049h");
        assert_geometry(38, false);
        runtime.test_process_pty_bytes(b"\x1b[?1049l");
        assert_geometry(37, true);
    }

    #[tokio::test]
    async fn lone_pane_scrollbar_gutter_is_reserved_before_scrollback_exists() {
        let mut app = AppState::test_new();
        let workspace = Workspace::test_new("test");
        let root_pane = workspace.tree().root();
        let terminal_runtimes = registry_with_runtime(
            &workspace,
            root_pane,
            PaneRuntime::test_with_scrollback_bytes(40, 8, 1024, b"ready\n"),
        );
        app.test_set_workspaces(vec![workspace]);
        app.seed_bookmark_index(Some(0));

        let area = Rect::new(10, 3, 40, 8);
        let infos = compute_pane_infos(&app, &terminal_runtimes, area);
        let info = &infos[0];

        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, None);
        assert_eq!(info.inner_rect, Rect::new(11, 4, 37, 6));
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
        app.test_set_workspaces(vec![workspace]);
        app.seed_bookmark_index(Some(0));

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
        let root_pane = workspace.tree().root();
        let terminal_runtimes = registry_with_runtime(
            &workspace,
            root_pane,
            PaneRuntime::test_with_scrollback_bytes(4, 8, 1024, b"ready\n"),
        );
        app.test_set_workspaces(vec![workspace]);
        app.seed_bookmark_index(Some(0));

        let area = Rect::new(10, 3, 4, 8);
        let infos = compute_pane_infos(&app, &terminal_runtimes, area);
        let info = &infos[0];

        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, None);
        assert_eq!(info.inner_rect, Rect::new(11, 4, 2, 6));
    }

    #[tokio::test]
    async fn rendered_content_rect_matches_the_size_new_panes_are_spawned_at() {
        for (pane_scrollbars, zoomed) in
            [(true, false), (false, false), (true, true), (false, true)]
        {
            let mut app = AppState::test_new();
            app.settings_mut().pane_scrollbars = pane_scrollbars;
            let area = Rect::new(2, 1, 101, 31);
            let mut workspace = Workspace::test_new("test");
            let root = workspace.tree().root();
            let right = workspace.test_split(shepr_core::layout::Direction::Horizontal);
            workspace.set_zoomed(zoomed);
            let mut terminal_runtimes = PaneRuntimeRegistry::default();
            for pane in [root, right] {
                terminal_runtimes.insert(
                    pane,
                    PaneRuntime::test_with_scrollback_bytes(20, 5, 1024, b""),
                );
            }
            app.test_set_workspaces(vec![workspace]);
            app.seed_bookmark_index(Some(0));
            app.test_record_all_workspace_areas(area);

            let infos = compute_pane_infos(&app, &terminal_runtimes, area);
            let geometry = app.chrome_in(app.layout_area(app.ws(0)));
            assert_eq!(geometry.area, crate::ui::core_rect(area));
            assert_eq!(infos.len(), if zoomed { 1 } else { 2 });
            for info in &infos {
                assert_eq!(
                    geometry.pane_size(app.ws(0).tree().layout(), zoomed, info.id),
                    shepr_core::geometry::GridSize::new(
                        info.inner_rect.width,
                        info.inner_rect.height
                    ),
                    "scrollbars {pane_scrollbars}, zoomed {zoomed}"
                );
            }
            for (_, runtime) in terminal_runtimes.drain() {
                drop(runtime);
            }
        }
    }

    #[tokio::test]
    async fn pane_scrollbar_setting_controls_reserved_column() {
        let mut app = AppState::test_new();
        let workspace = Workspace::test_new("test");
        let root_pane = workspace.tree().root();
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
        app.test_set_workspaces(vec![workspace]);
        app.seed_bookmark_index(Some(0));

        let area = Rect::new(10, 3, 40, 8);
        let infos = compute_pane_infos(&app, &terminal_runtimes, area);
        let info = &infos[0];

        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, Some(Rect::new(48, 4, 1, 6)));
        assert_eq!(info.inner_rect, Rect::new(11, 4, 37, 6));

        app.settings_mut().pane_scrollbars = false;
        let infos = compute_pane_infos(&app, &terminal_runtimes, area);
        let info = &infos[0];

        assert_eq!(info.rect, area);
        assert_eq!(info.scrollbar_rect, None);
        assert_eq!(info.inner_rect, Rect::new(11, 4, 38, 6));
    }
}

#[cfg(test)]
mod split_hit_tests {
    use super::*;

    #[test]
    fn split_hits_follow_divider_and_gap_geometry() {
        let horizontal = shepr_core::layout::SplitBorder {
            pos: 20,
            direction: shepr_core::layout::Direction::Horizontal,
            ratio: shepr_core::layout::SplitRatio::EVEN,
            area: shepr_core::geometry::Rect::new(2, 3, 40, 12),
            path: vec![shepr_core::layout::SplitBranch::First].into(),
        };
        assert_eq!(split_hit_rect(&horizontal, false), Rect::new(20, 3, 1, 12));
        assert_eq!(split_hit_rect(&horizontal, true), Rect::new(19, 3, 2, 12));

        let vertical = shepr_core::layout::SplitBorder {
            pos: 9,
            direction: shepr_core::layout::Direction::Vertical,
            ratio: shepr_core::layout::SplitRatio::EVEN,
            area: shepr_core::geometry::Rect::new(2, 3, 40, 12),
            path: vec![shepr_core::layout::SplitBranch::Second].into(),
        };
        assert_eq!(split_hit_rect(&vertical, true), Rect::new(2, 8, 40, 2));
        assert_eq!(split_hit_rect(&vertical, false), Rect::new(2, 9, 40, 1));

        let edge = shepr_core::layout::SplitBorder {
            pos: 0,
            direction: shepr_core::layout::Direction::Horizontal,
            ratio: shepr_core::layout::SplitRatio::EVEN,
            area: shepr_core::geometry::Rect::new(0, 0, 1, 4),
            path: shepr_core::layout::SplitPath::default(),
        };
        assert_eq!(split_hit_rect(&edge, true), Rect::new(0, 0, 1, 4));
    }
}
