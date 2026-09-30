use super::*;
use crate::tests::{test_pane_id, test_workspace_id};
use crossterm::event::MouseEvent;
use shepr_protocol::AgentStatus;
use shepr_protocol::command::{EndpointCommand, EndpointReply};
use shepr_protocol::{
    ClientShellAgent, ClientShellPane, PaneSurfacePane, PaneSurfaceSplit,
    PaneSurfaceSplitDirection, SurfaceRect,
};
use shepr_test_fixtures::*;
mod text_editing;

pub(super) fn snapshot() -> ClientShellSnapshot {
    ClientShellSnapshot {
        boot_id: crate::tests::test_boot_id("boot-1"),
        revision: shepr_protocol::ProjectionRevision::new(1),
        resolved_config: shepr_test_fixtures::encode_to_vec(
            &shepr_config::ValidatedConfig::test_default(),
        )
        .expect("test config encodes"),
        focused_workspace_id: Some(test_workspace_id("w1")),
        focused_pane_id: Some(test_pane_id("w1:p1")),
        workspaces: vec![ClientShellWorkspace {
            workspace_id: test_workspace_id("w1"),
            new_workspace_cwd: "/repo".into(),
            number: 1,
            label: "client-shell".into(),
            custom_label: false,
            branch: Some("main".into()),
            git_ahead_behind: None,
            focused: true,
            agent_status: AgentStatus::Idle,
        }],
        panes: vec![ClientShellPane {
            pane_id: test_pane_id("w1:p1"),
            workspace_id: test_workspace_id("w1"),
            label: None,
            cwd: Some("/repo".into()),
            foreground_cwd: Some("/repo".into()),
            focused: true,
            right_click_passthrough: false,
        }],
        agents: Vec::new(),
    }
}

fn surface() -> PaneSurfaceFrame {
    let surface_buffer = Buffer::with_lines(["LIVE", "PANE"]);
    PaneSurfaceFrame {
        boot_id: crate::tests::test_boot_id("boot-1"),
        projection_revision: shepr_protocol::ProjectionRevision::new(1),
        surface_revision: shepr_protocol::SurfaceRevision::new(1),
        frame: FrameData::from_ratatui_buffer_with_hyperlinks(
            &surface_buffer,
            Some(shepr_protocol::CursorState {
                x: 1,
                y: 1,
                visible: true,
                shape: shepr_protocol::CursorShapeParam::SteadyBlock,
            }),
            &[],
        ),
        panes: vec![PaneSurfacePane {
            pane_id: test_pane_id("w1:p1"),
            content_revision: 0,
            rect: SurfaceRect {
                x: 0,
                y: 0,
                width: 4,
                height: 2,
            },
            inner_rect: SurfaceRect {
                x: 0,
                y: 0,
                width: 4,
                height: 2,
            },
            scrollbar_rect: None,
            // A live pane always reports its scroll position; selections
            // need it to map viewport rows to absolute rows.
            scroll: Some(shepr_protocol::PaneSurfaceScrollMetrics {
                offset_from_bottom: 0,
                max_offset_from_bottom: 0,
                viewport_rows: 2,
                history_origin: shepr_vt::AbsRow(0),
            }),
            focused: true,
            mouse_reporting: false,
            sgr_pixel_mouse: false,
            alternate_screen_active: false,
            pixel_width: 0,
            pixel_height: 0,
        }],
        splits: Vec::new(),
    }
}

fn frame_rows(frame: &FrameData) -> Vec<String> {
    frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol.as_str()).collect())
        .collect()
}

/// Absolute cell position of `needle` inside `area`, for style assertions.
fn cell_symbol_position(frame: &FrameData, area: Rect, needle: &str) -> (u16, u16) {
    let rows = frame_rows(frame);
    for y in area.y..area.bottom().min(frame.height) {
        let row = &rows[y as usize];
        let slice = row
            .chars()
            .skip(area.x as usize)
            .take(area.width as usize)
            .collect::<String>();
        if let Some(byte) = slice.find(needle) {
            let column = u16::try_from(slice[..byte].chars().count()).unwrap_or(u16::MAX) + area.x;
            return (column, y);
        }
    }
    let visible = (area.y..area.bottom().min(frame.height))
        .map(|y| {
            rows[y as usize]
                .chars()
                .skip(area.x as usize)
                .take(area.width as usize)
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    panic!("symbol {needle:?} not found in {area:?}: {visible:?}");
}

fn pane_scroll_result(
    offset_from_bottom: u64,
    max_offset_from_bottom: u64,
    viewport_rows: u64,
) -> EndpointReply {
    EndpointReply::PaneInfo {
        pane: Box::new(shepr_protocol::command::PaneInfo {
            pane_id: shepr_test_fixtures::id("w1:p1"),
            focused: true,
            scroll: Some(shepr_protocol::command::PaneScrollInfo {
                offset_from_bottom,
                max_offset_from_bottom,
                viewport_rows,
            }),
        }),
    }
}

fn copy_search_result(
    matches: Vec<shepr_protocol::command::PaneTextRange>,
    current: Option<u32>,
) -> EndpointReply {
    let total = matches.len() as u64;
    EndpointReply::PaneCopySearch {
        pane_id: "w1:p1".into(),
        matches,
        total,
        current,
        current_global: current.map(u64::from),
    }
}

mod chrome_context;
mod copy;
mod endpoint_requests;
mod endpoints;
#[path = "input.rs"]
mod input_domain;
mod mouse_selection;
mod startup_overlays;
