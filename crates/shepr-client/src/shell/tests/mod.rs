//! Fixtures shared by the shell's tests, and the tests that exercise the shell as a
//! whole rather than one of its components. A component's own tests, including those
//! that drive it through the whole shell, sit with the component. The few fixtures the
//! endpoint hub's tests drive the shell with are crate-visible.

mod chrome_context;
mod endpoints;
mod input_domain;
mod presentation_regressions;
mod startup_overlays;
mod text_editing;
mod workspace_navigation;

use crate::endpoint::ClientEndpointId;
use crate::shell::config::ClientShellConfig;
use crate::shell::overlays::Overlay;
use crate::shell::overlays::help::HelpOverlay;
use crate::shell::overlays::rename::RenameTarget;
use crate::shell::state::{
    ClientShellAction, ClientShellEndpointError, ClientShellInput, ClientShellMode,
    ClientShellState,
};
use crate::tests::{test_pane_id, test_workspace_id};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use shepr_config::ClientConfig;
use shepr_protocol::command::EndpointReply;
use shepr_protocol::{
    AgentStatus, ClientShellAgent, ClientShellPane, ClientShellSnapshot, ClientShellWorkspace,
    FrameData, PaneSurfaceFrame, PaneSurfacePane, SurfaceRect,
};
use shepr_surface::ratatui_conversion::{FrameDataExt as _, WireColorExt as _};
use shepr_termio::text_editor::TextEditor;

pub(in crate::shell) fn snapshot() -> ClientShellSnapshot {
    ClientShellSnapshot {
        boot_id: crate::tests::test_boot_id("boot-1"),
        restore_notice: None,
        session_saves_stopped: false,
        revision: shepr_protocol::ProjectionRevision::FIRST,
        focused_workspace_id: Some(test_workspace_id("w1")),
        focused_pane_id: Some(test_pane_id("w1:p1")),
        workspaces: vec![ClientShellWorkspace {
            workspace_id: test_workspace_id("w1"),
            new_workspace_cwd: Some("/repo".into()),
            label: "client-shell".into(),
            branch: Some("main".into()),
            git_ahead_behind: None,
            agent_status: AgentStatus::Idle,
        }],
        panes: vec![ClientShellPane {
            pane_id: test_pane_id("w1:p1"),
            label: None,
            cwd: Some("/repo".into()),
            foreground_cwd: Some("/repo".into()),
            right_click_passthrough: false,
        }],
        agents: Vec::new(),
    }
}

pub(in crate::shell) fn surface() -> PaneSurfaceFrame {
    let surface_buffer = Buffer::with_lines(["LIVE", "PANE"]);
    PaneSurfaceFrame {
        boot_id: crate::tests::test_boot_id("boot-1"),
        projection_revision: shepr_protocol::ProjectionRevision::FIRST,
        surface_revision: shepr_protocol::SurfaceRevision::FIRST,
        frame: FrameData::from_ratatui_buffer_with_hyperlinks(
            &surface_buffer,
            Some(shepr_protocol::CursorState {
                x: 1,
                y: 1,
                visible: true,
                shape: shepr_protocol::CursorShapeParam::SteadyBlock,
            }),
            &[],
        )
        .expect("test buffer is a valid frame"),
        panes: vec![PaneSurfacePane {
            pane_id: test_pane_id("w1:p1"),
            content_revision: shepr_protocol::ContentRevision::default(),
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
            scroll: Some(shepr_protocol::PaneSurfaceScrollMetrics::new(
                0,
                0,
                2,
                shepr_term::AbsRow(0),
            )),
            focused: true,
            mouse_reporting: false,
            pixel_mouse: shepr_term::mouse::PanePixelMouse::OFF,
            alternate_screen_active: false,
        }],
        splits: Vec::new(),
    }
}

pub(in crate::shell) fn frame_rows(frame: &FrameData) -> Vec<String> {
    frame
        .cells()
        .chunks(frame.width() as usize)
        .map(|row| row.iter().map(|cell| cell.symbol.as_str()).collect())
        .collect()
}

/// The wire cell at `position` of `frame`. Composed frames are read as wire cells, never
/// converted back to a ratatui buffer.
pub(in crate::shell) fn frame_cell(
    frame: &FrameData,
    (x, y): (u16, u16),
) -> &shepr_protocol::CellData {
    frame.cell(x, y).unwrap_or_else(|| {
        panic!(
            "cell ({x}, {y}) is outside the {}x{} frame",
            frame.width(),
            frame.height()
        )
    })
}

pub(in crate::shell) fn cell_fg(frame: &FrameData, position: (u16, u16)) -> ratatui::style::Color {
    frame_cell(frame, position).fg.to_ratatui()
}

pub(in crate::shell) fn cell_bg(frame: &FrameData, position: (u16, u16)) -> ratatui::style::Color {
    frame_cell(frame, position).bg.to_ratatui()
}

pub(in crate::shell) fn cell_is_bold(frame: &FrameData, position: (u16, u16)) -> bool {
    frame_cell(frame, position)
        .style
        .flags
        .contains(shepr_protocol::WireStyleFlags::BOLD)
}

/// Absolute cell position of `needle` inside `area`, for style assertions.
pub(in crate::shell) fn cell_symbol_position(
    frame: &FrameData,
    area: Rect,
    needle: &str,
) -> (u16, u16) {
    let rows = frame_rows(frame);
    for y in area.y..area.bottom().min(frame.height()) {
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
    let visible = (area.y..area.bottom().min(frame.height()))
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

/// Presses Enter in the open overlay, which activates what it highlights.
pub(in crate::shell) fn press_overlay_enter(
    state: &mut ClientShellState,
    outcome: &mut ClientShellInput,
) {
    state.route_overlay_key(
        &shepr_term::key::TerminalKey::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ),
        outcome,
    );
}

/// Presses `code` in the open overlay, the way the input path routes a key to it.
pub(in crate::shell) fn press_overlay_key(
    state: &mut ClientShellState,
    code: crossterm::event::KeyCode,
) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.route_overlay_key(
        &shepr_term::key::TerminalKey::new(code, crossterm::event::KeyModifiers::NONE),
        &mut outcome,
    );
    outcome
}

/// Presses one key through the shell's whole input path.
pub(in crate::shell) fn press(
    state: &mut ClientShellState,
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> ClientShellInput {
    state.handle_raw_events(vec![shepr_termio::input::raw_input::RawInputEvent::Key(
        shepr_term::key::TerminalKey::new(code, modifiers),
    )])
}

/// Opens Help the way its key binding does.
pub(in crate::shell) fn open_help(state: &mut ClientShellState) {
    let mut open = ClientShellInput::default();
    state.record_binding(&shepr_termio::input::KeybindAction::Help, &mut open);
}

/// The open Help overlay, if Help is open.
pub(in crate::shell) fn help_overlay(state: &ClientShellState) -> Option<&HelpOverlay> {
    match state.overlay.as_ref() {
        Some(Overlay::Help(help)) => Some(help),
        _ => None,
    }
}

/// The target of the open name prompt, if one is open.
pub(in crate::shell) fn rename_target(state: &ClientShellState) -> Option<&RenameTarget> {
    match state.overlay.as_ref() {
        Some(Overlay::Rename(rename)) => Some(rename.target()),
        _ => None,
    }
}

/// A presented shell with one of the six one-line text fields open: 0 the new-workspace
/// prompt, 1 workspace rename, 2 pane rename, 3 the navigator search, 4 the Help search
/// and 5 the copy-mode search prompt. Each is opened the way a user opens it.
pub(in crate::shell) fn prompt_shell(field: usize) -> ClientShellState {
    let mut state = presented_shell();
    state.compose(106, 30).expect("initial shell");
    match field {
        0 => state.open_new_workspace_overlay(),
        1 => state.open_rename_workspace_overlay(),
        2 => state.open_rename_pane_overlay(),
        3 => {
            state.open_navigator_overlay();
            state.handle_input_bytes(b"/");
        }
        4 => {
            open_help(&mut state);
            state.handle_input_bytes(b"/");
        }
        5 => {
            state.record_binding(
                &shepr_termio::input::KeybindAction::CopyMode,
                &mut ClientShellInput::default(),
            );
            state.handle_input_bytes(b"/");
        }
        _ => unreachable!(),
    }
    state
}

/// The editor of the text field that has input: the open overlay's, or else the copy-mode
/// search prompt's.
pub(in crate::shell) fn prompt_text(state: &ClientShellState) -> &TextEditor {
    match state.overlay.as_ref() {
        Some(Overlay::Rename(v)) => v.input(),
        Some(Overlay::Navigator(v)) => &v.query,
        Some(Overlay::Help(v)) => v.query(),
        _ => {
            &state
                .copy
                .as_ref()
                .expect("copy mode")
                .search
                .as_ref()
                .expect("search state")
                .prompt
                .as_ref()
                .expect("prompt")
                .query
        }
    }
}

/// Replaces the text of the field that has input the way a user does: kill everything
/// before the end, then paste `text`, which leaves the cursor at its end.
pub(in crate::shell) fn fill_prompt(state: &mut ClientShellState, text: &str) {
    press(
        state,
        crossterm::event::KeyCode::End,
        crossterm::event::KeyModifiers::NONE,
    );
    press(
        state,
        crossterm::event::KeyCode::Char('u'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    state.handle_raw_events(vec![shepr_termio::input::raw_input::RawInputEvent::Paste(
        text.into(),
    )]);
    assert_eq!(
        prompt_text(state).as_str(),
        text,
        "the field holds the filled text"
    );
}

/// Feeds `bytes` in navigate mode, where each key only moves the preview.
pub(in crate::shell) fn preview_key(state: &mut ClientShellState, bytes: &[u8]) {
    let outcome = state.handle_input_bytes(bytes);
    assert!(outcome.actions.is_empty(), "{bytes:?}");
    assert!(outcome.requests.is_empty(), "{bytes:?}");
    assert!(outcome.repaint, "{bytes:?}");
}

/// Enters navigate mode with the default prefix and `w`.
pub(in crate::shell) fn enter_navigation(state: &mut ClientShellState) {
    preview_key(state, &[0x02]);
    preview_key(state, b"w");
    assert_eq!(state.mode.kind(), ClientShellMode::Navigate);
}

pub(in crate::shell) fn pane_scroll_result(
    offset_from_bottom: usize,
    max_offset_from_bottom: usize,
    viewport_rows: usize,
) -> EndpointReply {
    EndpointReply::PaneInfo {
        pane: Box::new(shepr_protocol::command::PaneInfo {
            pane_id: shepr_test_fixtures::id("w1:p1"),
            scroll: Some(shepr_protocol::command::PaneScrollInfo::new(
                offset_from_bottom,
                max_offset_from_bottom,
                viewport_rows,
                shepr_term::AbsRow(0),
            )),
        }),
    }
}

pub(in crate::shell) fn copy_search_result(
    matches: Vec<shepr_protocol::command::PaneTextRange>,
    current: Option<usize>,
) -> EndpointReply {
    EndpointReply::PaneCopySearch {
        pane_id: shepr_test_fixtures::id("w1:p1"),
        search: shepr_protocol::command::PaneCopySearch {
            total: matches.len(),
            matches,
            current: current.map(|index| shepr_protocol::command::PaneCopySearchPosition {
                window_index: index,
                global_index: index,
            }),
        },
    }
}

/// A shell with the default snapshot and surface and no frame drawn yet.
fn presented_shell() -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    state
}

/// A presented shell with one workspace rename submitted and its actions.
pub(crate) fn pending_request() -> (ClientShellState, Vec<ClientShellAction>) {
    let mut state = presented_shell();
    state.open_rename_workspace_overlay();
    state.handle_input_bytes(b"renamed");
    let outcome = state.handle_input_bytes(b"\r");
    (state, outcome.actions)
}

/// A presented shell with two requests submitted, and their actions in order: a pane
/// scroll (a read whose loss reports no interruption), then the workspace creation the
/// new-workspace prompt's Enter sends.
pub(crate) fn read_then_command() -> (ClientShellState, Vec<ClientShellAction>) {
    let mut state = ready_shell();
    let mut out = ClientShellInput::default();
    state.push_pane_scroll_offset(crate::tests::test_pane_id("w1:p1"), 3, &mut out);
    let mut actions = out.actions;
    state.open_new_workspace_overlay();
    actions.extend(state.handle_input_bytes(b"\r").actions);
    assert_eq!(actions.len(), 2);
    (state, actions)
}

/// The id of the one endpoint request in `actions`.
pub(crate) fn request_id(actions: &[ClientShellAction]) -> &shepr_protocol::RequestId {
    let [ClientShellAction::Endpoint { request, .. }] = actions else {
        panic!("expected one endpoint request");
    };
    &request.id
}

/// A presented shell with one frame drawn, so input has pane hits to aim at.
pub(in crate::shell) fn ready_shell() -> ClientShellState {
    let mut state = presented_shell();
    state.compose(106, 20).expect("compose");
    state
}

/// A ready shell in copy mode on its one pane.
pub(in crate::shell) fn copy_shell() -> ClientShellState {
    let mut state = ready_shell();
    assert!(state.enter_copy_mode(&mut ClientShellInput::default()));
    state
}

/// Submits a copy-mode search for "needle" and returns its request id.
pub(in crate::shell) fn copy_search(state: &mut ClientShellState) -> shepr_protocol::RequestId {
    let outcome = state.handle_input_bytes(b"/needle\r");
    request_id(&outcome.actions).to_owned()
}

/// Answers `id` as the default snapshot's server would.
pub(in crate::shell) fn answer(
    state: &mut ClientShellState,
    id: &shepr_protocol::RequestId,
    result: Result<EndpointReply, ClientShellEndpointError>,
) -> ClientShellInput {
    state.answer_request(
        &crate::tests::test_boot_id("boot-1"),
        id,
        result,
        std::time::Instant::now(),
    )
}

pub(in crate::shell) fn remote_machine() -> shepr_config::MachineConfig {
    machine_named("Build", "dev@build.example")
}

pub(in crate::shell) fn machine_named(label: &str, ssh: &str) -> shepr_config::MachineConfig {
    shepr_config::MachineConfig {
        label: shepr_config::MachineLabel::parse(label).expect("test precondition"),
        ssh: shepr_config::SshTarget::parse(ssh).expect("test precondition"),
        palette: None,
    }
}

/// The first argument only documents which agent a test means; agents carry
/// no name of their own.
pub(in crate::shell) fn agent(
    status: shepr_protocol::AgentStatus,
    state_change_seq: u64,
) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: "w1:p1".parse().expect("test precondition"),
        agent: Some(shepr_config::ConfigAgent::Pi),
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: status,
        state_change_seq: shepr_test_fixtures::counter_at(state_change_seq),
    }
}

pub(in crate::shell) fn snapshot_with_agent(
    boot_id: &str,
    pane_id: &str,
    status: shepr_protocol::AgentStatus,
    state_change_seq: u64,
) -> ClientShellSnapshot {
    let mut value = snapshot();
    let pane_id = test_pane_id(pane_id);
    value.boot_id = crate::tests::test_boot_id(boot_id);
    value.focused_pane_id = Some(pane_id);
    value.panes[0].pane_id = pane_id;
    value.agents = vec![ClientShellAgent {
        pane_id,
        ..agent(status, state_change_seq)
    }];
    value
}

pub(in crate::shell) fn state_with_remote() -> (ClientShellState, ClientEndpointId) {
    state_with_remote_config(&ClientConfig::default())
}

/// As `state_with_remote`, with the shell launched on `config`.
pub(in crate::shell) fn state_with_remote_config(
    config: &ClientConfig,
) -> (ClientShellState, ClientEndpointId) {
    state_with_machines_config(&[remote_machine()], config)
}

/// Online state with a remote snapshot for the first machine; any others stay Connecting.
pub(in crate::shell) fn state_with_machines(
    machines: &[shepr_config::MachineConfig],
) -> (ClientShellState, ClientEndpointId) {
    state_with_machines_config(machines, &ClientConfig::default())
}

fn state_with_machines_config(
    machines: &[shepr_config::MachineConfig],
    config: &ClientConfig,
) -> (ClientShellState, ClientEndpointId) {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(config));
    let endpoint_id = ClientEndpointId::Ssh(machines[0].label.clone());
    state.set_machines(machines);
    state.set_snapshot(Box::new(snapshot()));
    state.receive_pane_surface_from(
        surface(),
        state
            .endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    let mut remote = snapshot();
    remote.boot_id = crate::tests::test_boot_id("remote-boot");
    remote.workspaces[0].label = "remote-workspace".into();
    state.connect_endpoint_with_snapshot(&endpoint_id, 1, Box::new(remote));
    (state, endpoint_id)
}
