use ratatui::{Frame, layout::Rect};

use super::PaneResizer;
use super::panes::{compute_pane_infos_for_tab, render_panes, resize_pane_infos};
use crate::app::AppState;
use shepr_core::layout::SplitBorder;
use shepr_mux::pane::PaneRuntimeRegistry;
use shepr_mux::workspace::PaneChromeInfo as PaneInfo;
use shepr_protocol::CursorState;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TabSurfaceTarget {
    pub(crate) workspace_id: shepr_protocol::WorkspaceId,
    pub(crate) tab_id: shepr_protocol::PublicTabId,
}

impl TabSurfaceTarget {
    pub(crate) fn from_indices(
        app: &AppState,
        workspace_index: usize,
        tab_index: usize,
    ) -> Option<Self> {
        let workspace = app.workspaces.get(workspace_index)?;
        let tab = workspace.tabs().get(tab_index)?;
        Some(Self {
            workspace_id: workspace.id.clone(),
            tab_id: shepr_protocol::PublicTabId::new(&workspace.id, tab.number()),
        })
    }

    pub(crate) fn resolve(&self, app: &AppState) -> Option<(usize, usize)> {
        if *self.tab_id.workspace_id() != self.workspace_id {
            return None;
        }
        let workspace_index = app
            .workspaces
            .iter()
            .position(|workspace| workspace.id == self.workspace_id)?;
        let workspace = &app.workspaces[workspace_index];
        let tab_index = workspace
            .tabs()
            .iter()
            .position(|tab| tab.number() == self.tab_id.number())?;
        Some((workspace_index, tab_index))
    }
}

pub(crate) struct TabSurfaceLayout {
    pub(crate) target: Option<TabSurfaceTarget>,
    pub(crate) pane_infos: Vec<PaneInfo>,
    pub(crate) split_borders: Vec<SplitBorder>,
}

#[derive(Clone, Copy)]
pub(crate) struct TabSurfaceView<'a> {
    pub(crate) target: Option<&'a TabSurfaceTarget>,
    pub(crate) pane_infos: &'a [PaneInfo],
    pub(crate) split_borders: &'a [SplitBorder],
}

/// One tab laid out for one client surface of size `area`. Pure: it reads the
/// state and the runtimes' screen modes and resizes nothing. Each client
/// renders the tab its own location names at its own size; which size the
/// tab's PTYs get is decided separately, by the server's geometry rule.
pub(crate) fn compute_tab_surface_for(
    app: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
    target: Option<TabSurfaceTarget>,
    area: Rect,
) -> TabSurfaceLayout {
    let resolved = target.as_ref().and_then(|target| target.resolve(app));
    let tab = resolved.and_then(|(workspace_index, tab_index)| {
        app.workspaces.get(workspace_index)?.tabs().get(tab_index)
    });
    let split_borders = tab.map_or_default(|tab| {
        if tab.zoomed() {
            Vec::new()
        } else {
            tab.layout().splits(shepr_mux::workspace::layout_rect(area))
        }
    });
    let pane_infos = resolved.map_or_else(Vec::new, |(workspace_index, tab_index)| {
        compute_pane_infos_for_tab(app, terminal_runtimes, workspace_index, tab_index, area)
    });

    TabSurfaceLayout {
        target,
        pane_infos,
        split_borders,
    }
}

/// Resizes the visible panes of one tab to their content rects in `area`: the
/// explicit geometry path the server's PTY size rule runs through.
pub(crate) fn resize_tab_surface(
    app: &AppState,
    resizer: &PaneResizer<'_>,
    workspace_index: usize,
    tab_index: usize,
    area: Rect,
    cell_size: shepr_termio::host_term::cell_size::HostCellSize,
) {
    let pane_infos =
        compute_pane_infos_for_tab(app, resizer.runtimes, workspace_index, tab_index, area);
    resize_pane_infos(
        app,
        resizer,
        workspace_index,
        tab_index,
        &pane_infos,
        cell_size,
    );
}

pub(crate) fn render_tab_surface(
    app: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
    surface: TabSurfaceView<'_>,
    frame: &mut Frame<'_>,
) {
    render_panes(
        app,
        terminal_runtimes,
        frame,
        surface.target,
        surface.pane_infos,
        surface.split_borders,
    );
}

pub(crate) fn tab_surface_hyperlinks(
    app: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
    surface: TabSurfaceView<'_>,
) -> Vec<((u16, u16), String, String)> {
    let Some((ws_idx, _)) = surface.target.and_then(|target| target.resolve(app)) else {
        return Vec::new();
    };
    if app.workspaces.get(ws_idx).is_none() {
        return Vec::new();
    }

    let mut links = Vec::new();
    for info in surface.pane_infos {
        if let Some(runtime) = app.runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, info.id)
        {
            links.extend(runtime.visible_hyperlinks(info.inner_rect));
        }
    }
    links
}

pub(crate) fn tab_surface_cursor(
    app: &AppState,
    terminal_runtimes: &PaneRuntimeRegistry,
    surface: TabSurfaceView<'_>,
) -> Option<CursorState> {
    let (ws_idx, _) = surface.target?.resolve(app)?;
    let info = surface.pane_infos.iter().find(|info| info.is_focused)?;
    let runtime = app.runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, info.id)?;
    if runtime.synchronized_output_active() {
        return None;
    }
    let scrolled_back = super::panes::pane_is_scrolled_back(runtime);
    let reveal = app.settings.reveal_hidden_cursor_for_cjk_ime
        && (app.settings.cjk_ime_agents.is_empty() || {
            let detected = app
                .workspaces
                .get(ws_idx)
                .and_then(|ws| ws.terminal_id(info.id))
                .and_then(|terminal_id| app.terminals.get(terminal_id))
                .and_then(|terminal| terminal.detected_agent);
            detected.is_some_and(|agent| {
                app.settings
                    .cjk_ime_agents
                    .iter()
                    .any(|configured| configured.label() == agent.label())
            })
        });

    if let Some(cursor) = runtime.cursor_state(info.inner_rect, true) {
        let visible = if reveal {
            !scrolled_back
        } else {
            cursor.visible && !scrolled_back
        };
        Some(CursorState {
            x: cursor.x,
            y: cursor.y,
            visible,
            shape: if reveal && visible {
                shepr_protocol::CursorShapeParam::from_decscusr(app.settings.cjk_ime_cursor_shape)
            } else {
                cursor.shape
            },
        })
    } else if reveal && !scrolled_back {
        Some(CursorState {
            x: info.inner_rect.x,
            y: info.inner_rect.y,
            visible: true,
            shape: shepr_protocol::CursorShapeParam::from_decscusr(
                app.settings.cjk_ime_cursor_shape,
            ),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use shepr_core::layout::Direction;
    use shepr_mux::workspace::Workspace;

    #[test]
    fn target_tracks_tab_across_position_changes() {
        let mut app = AppState::test_new();
        let first = Workspace::test_new("first");
        let mut second = Workspace::test_new("second");
        second.test_add_tab(Some("other"));
        app.workspaces = vec![first, second];

        let target = TabSurfaceTarget::from_indices(&app, 1, 1).expect("test precondition");
        app.workspaces.swap(0, 1);
        assert!(app.workspaces[0].move_tab(0, 2));
        assert_eq!(target.resolve(&app), Some((0, 0)));

        assert!(app.workspaces[0].close_tab(0).is_some());
        assert_eq!(target.resolve(&app), None);
    }

    #[tokio::test]
    async fn explicit_surface_layout_drives_render_cursor_and_hyperlinks() {
        let uri = "https://example.com/surface";
        let mut workspace = Workspace::test_new("shell-workspace");
        let left = workspace.tabs()[0].root_pane();
        let right = workspace.test_split(Direction::Horizontal);
        let mut runtimes = PaneRuntimeRegistry::new();
        let left_terminal = workspace.terminal_id(left).cloned().expect("left terminal");
        let right_terminal = workspace
            .terminal_id(right)
            .cloned()
            .expect("right terminal");
        runtimes.insert(
            left_terminal,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(
                20,
                8,
                format!("\x1b]8;;{uri}\x1b\\LEFT\x1b]8;;\x1b\\").as_bytes(),
            ),
        );
        runtimes.insert(
            right_terminal,
            shepr_mux::pane::PaneRuntime::test_with_screen_bytes(20, 8, b"RIGHT"),
        );

        let mut app = AppState::test_new();
        app.workspaces = vec![workspace];
        app.set_active_index(Some(0));
        app.set_selected_index(Some(0));

        let full_area = Rect::new(0, 0, 106, 20);
        let area = full_area;
        let surface = compute_tab_surface_for(
            &app,
            &runtimes,
            TabSurfaceTarget::from_indices(&app, 0, 0),
            area,
        );
        assert_eq!(surface.pane_infos.len(), 2);
        assert!(!surface.split_borders.is_empty());

        // The recorded layout area of the tab is session geometry for spawn
        // sizing; a client surface is drawn from its own layout alone.
        app.test_record_all_tab_areas(Rect::new(9, 8, 7, 6));

        let surface_view = TabSurfaceView {
            target: surface.target.as_ref(),
            pane_infos: &surface.pane_infos,
            split_borders: &surface.split_borders,
        };
        let mut terminal = Terminal::new(TestBackend::new(full_area.width, full_area.height))
            .expect("test precondition");
        terminal
            .draw(|frame| {
                render_tab_surface(&app, &runtimes, surface_view, frame);
            })
            .expect("test precondition");

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("LEFT"), "surface: {rendered:?}");
        assert!(rendered.contains("RIGHT"), "surface: {rendered:?}");
        assert!(!rendered.contains("shell-workspace"));

        let links = tab_surface_hyperlinks(&app, &runtimes, surface_view);
        assert!(
            links
                .iter()
                .any(|(_, symbol, link)| { symbol == "L" && link == uri })
        );
        assert!(tab_surface_cursor(&app, &runtimes, surface_view,).is_some());
    }
}
