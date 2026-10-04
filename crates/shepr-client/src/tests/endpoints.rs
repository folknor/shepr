//! Endpoint fixtures: a transport that records what the client sends, and the snapshot and
//! surface a test server named Local or `build` sends.

use super::{test_boot_id, test_pane_id, test_workspace_id};
use crate::endpoint::{ClientEndpointId, EndpointTransport};
use shepr_protocol::{ClientMessage, ClientShellSnapshot, ClientSurfaceSize, PaneSurfaceFrame};
use shepr_surface::ratatui_conversion::FrameDataExt as _;
use shepr_test_fixtures::counter_at;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Keeps every message sent through it; `fail_next` makes the next send fail.
#[derive(Clone, Default)]
pub(crate) struct RecordingTransport {
    pub(crate) sent: Arc<Mutex<Vec<ClientMessage>>>,
    pub(crate) fail: Arc<AtomicBool>,
}

impl RecordingTransport {
    pub(crate) fn take(&self) -> Vec<ClientMessage> {
        std::mem::take(&mut *self.sent.lock().expect("messages"))
    }

    pub(crate) fn fail_next(&self) {
        self.fail.store(true, Ordering::Release);
    }
}

impl EndpointTransport for RecordingTransport {
    fn send(&mut self, message: &ClientMessage) -> io::Result<()> {
        if self.fail.swap(false, Ordering::AcqRel) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "recording send failed",
            ));
        }
        self.sent
            .lock()
            .map_err(|_| io::Error::other("messages lock"))?
            .push(message.clone());
        Ok(())
    }

    fn disconnect(&mut self) {}

    fn flush(&mut self, _deadline: Instant) -> io::Result<()> {
        Ok(())
    }

    fn take_error(&mut self) -> Option<io::Error> {
        None
    }
}

/// The configured machine the endpoint tests move to.
pub(crate) fn remote() -> ClientEndpointId {
    ClientEndpointId::Ssh(shepr_config::MachineLabel::parse("build").expect("machine"))
}

pub(crate) fn boot(id: &ClientEndpointId) -> shepr_protocol::BootId {
    test_boot_id(if id.is_local() {
        "local-boot"
    } else {
        "remote-boot"
    })
}

/// One workspace `w1` with one focused pane `w1:p1`, at projection revision `revision`.
pub(crate) fn snapshot(id: &ClientEndpointId, revision: u64) -> Box<ClientShellSnapshot> {
    Box::new(ClientShellSnapshot {
        boot_id: boot(id),
        revision: counter_at(revision),
        restore_notice: None,
        session_saves_stopped: false,
        focused_workspace_id: Some(test_workspace_id("w1")),
        focused_pane_id: Some(test_pane_id("w1:p1")),
        workspaces: vec![shepr_protocol::ClientShellWorkspace {
            workspace_id: test_workspace_id("w1"),
            new_workspace_cwd: Some("/repo".into()),
            label: match id {
                ClientEndpointId::Local => "local".into(),
                ClientEndpointId::Ssh(label) => label.as_str().into(),
            },
            branch: None,
            git_ahead_behind: None,
            agent_status: shepr_protocol::AgentStatus::Idle,
        }],
        panes: vec![shepr_protocol::ClientShellPane {
            pane_id: test_pane_id("w1:p1"),
            label: None,
            cwd: Some("/repo".into()),
            foreground_cwd: None,
            right_click_passthrough: false,
        }],
        agents: vec![],
    })
}

/// The surface of `snapshot(id, revision)`: pane `w1:p1` filling `size`, with `marker` drawn
/// at its top left.
pub(crate) fn surface(
    id: &ClientEndpointId,
    revision: u64,
    size: ClientSurfaceSize,
    marker: &str,
) -> PaneSurfaceFrame {
    let area = ratatui::layout::Rect::new(0, 0, size.cols, size.rows);
    let mut buffer = ratatui::buffer::Buffer::empty(area);
    buffer.set_string(0, 0, marker, ratatui::style::Style::default());
    PaneSurfaceFrame {
        boot_id: boot(id),
        projection_revision: counter_at(revision),
        surface_revision: counter_at(revision),
        frame: shepr_protocol::FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[])
            .expect("test surface size is valid"),
        panes: vec![shepr_protocol::PaneSurfacePane {
            pane_id: test_pane_id("w1:p1"),
            content_revision: counter_at(revision),
            rect: shepr_protocol::SurfaceRect {
                x: 0,
                y: 0,
                width: size.cols,
                height: size.rows,
            },
            inner_rect: shepr_protocol::SurfaceRect {
                x: 0,
                y: 0,
                width: size.cols,
                height: size.rows,
            },
            scrollbar_rect: None,
            scroll: None,
            focused: true,
            mouse_reporting: false,
            pixel_mouse: shepr_term::mouse::PanePixelMouse::OFF,
            alternate_screen_active: false,
        }],
        splits: vec![],
    }
}
