use crate::errors::LoopExit;
use crate::state::ClientState;
use crate::terminal_geometry::{query_host_terminal_appearance, query_host_terminal_theme};
use crate::{endpoint, shell};
use shepr_protocol::ClientMessage;
use tracing::warn;

/// The geometry every endpoint is asked to render: the host size under the client's own layout.
/// Shell chrome belongs to the client, so all endpoints lay out the same surface. Handshakes
/// and resize messages use this producer so they cannot disagree about pixel bounding.
pub(super) fn view_geometry(
    host: shepr_core::geometry::HostGeometry,
    size: shepr_protocol::ClientSurfaceSize,
) -> shepr_protocol::TerminalGeometry {
    shepr_protocol::TerminalGeometry::from_host(
        shepr_core::geometry::GridSize::clamped(size.cols, size.rows),
        host.cell(),
    )
}

/// Sends the current geometry to every viewed connection through the hub.
pub(super) fn resize_views(state: &mut ClientState, hub: &mut endpoint::EndpointHub) {
    let geometry = view_geometry(
        state.reported_geometry,
        state.shell.surface_size(
            state.reported_geometry.cols(),
            state.reported_geometry.rows(),
        ),
    );
    hub.resize_views(&mut state.shell, geometry);
}

pub(super) fn sync_client_shell_keyboard_report_all(
    state: &mut ClientState,
) -> Result<(), LoopExit> {
    let result = state.host_modes.sync_shell_keyboard_report_all(
        &mut state.output_writer,
        state.shell.host_keyboard_report_all_requested(),
    );
    state.record_host_mode_write("keyboard report-all", result)
}

/// Drops host terminal effects requested by a lost or retired endpoint. Every step runs even
/// after one fails, so a failed mouse reset still clears report-all. Each result passes
/// through the shared policy: a transient failure queues a retry, while a permanent
/// stateful-mode failure ends the client after all resets have been attempted. The window
/// title is the client's own, not an endpoint's, so it stays.
pub(super) fn clear_endpoint_host_effects(state: &mut ClientState) -> Result<(), LoopExit> {
    state.host_modes.clear_mouse_endpoint_request();
    let mouse = state
        .host_modes
        .apply_mouse(&mut state.output_writer, state.reported_geometry.cell());
    let shell_requests_report_all = state.shell.host_keyboard_report_all_requested();
    state.host_modes.set_pane_keyboard_report_all(false);
    let report_all = state
        .host_modes
        .sync_shell_keyboard_report_all(&mut state.output_writer, shell_requests_report_all);
    let mouse = state.record_host_mode_write("mouse capture reset", mouse);
    let report_all = state.record_host_mode_write("keyboard report-all reset", report_all);
    mouse.and(report_all)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ShellInputDisposition {
    Continue,
    Detach,
}

pub(super) fn finish_client_shell_input(
    state: &mut ClientState,
    outcome: shell::ClientShellInput,
    hub: &mut endpoint::EndpointHub,
    now: std::time::Instant,
) -> Result<ShellInputDisposition, LoopExit> {
    if outcome.detach {
        hub.detach(&state.shell);
        return Ok(ShellInputDisposition::Detach);
    }
    let repaint = outcome.repaint;
    if outcome.resize {
        resize_views(state, hub);
    }
    if outcome.full_redraw {
        // Discard the blit baseline so the pending frame is written in full; the
        // host may have lost or garbled what we last drew while unfocused.
        state.request_repaint();
    }
    if outcome.query_host_appearance {
        query_host_terminal_appearance(&mut state.output_writer).map_err(LoopExit::HostTerminal)?;
    }
    if outcome.query_host_theme {
        query_host_terminal_theme(&mut state.output_writer).map_err(LoopExit::HostTerminal)?;
    }
    sync_client_shell_keyboard_report_all(state)?;
    let dispatched = hub.dispatch(&mut state.shell, outcome.actions, now);
    for bytes in dispatched.clipboard {
        // Once per user copy, so a warn cannot flood; only the length is
        // logged because the bytes are the user's selection.
        if let Err(error) = shepr_termio::host_term::title::write_clipboard_bytes(
            &bytes,
            state.settings.clipboard_route(),
            &mut state.output_writer,
        ) {
            warn!(
                bytes = bytes.len(),
                %error,
                "clipboard copy did not reach the host clipboard"
            );
        }
    }
    for request in outcome.requests {
        match request {
            shell::ClientShellRequest::HostTheme(update) => {
                state.record_host_theme_update(&update);
                hub.send_viewed(&ClientMessage::ClientShellHostTheme { update });
            }
            shell::ClientShellRequest::Shown(request) => {
                hub.send_shown(&state.shell, &request);
            }
        }
    }
    if repaint || dispatched.repaint.is_needed() {
        state.mark_pane_dirty();
    }
    Ok(ShellInputDisposition::Continue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn transport_failures_map_to_fixed_disconnect_notices() {
        assert_eq!(
            shepr_launch::EndpointFailure::from_error(&io::Error::from(
                io::ErrorKind::UnexpectedEof
            ))
            .disconnect_notice(),
            "connection was lost; reconnecting"
        );
        assert_eq!(
            shepr_launch::EndpointFailure::from_error(&io::Error::from(io::ErrorKind::TimedOut))
                .disconnect_notice(),
            "connection timed out; reconnecting"
        );
        assert_eq!(
            shepr_launch::EndpointFailure::from_error(&io::Error::from(io::ErrorKind::InvalidData))
                .disconnect_notice(),
            "connection failed; needs attention"
        );
    }

    #[test]
    fn window_title_reset_only_undoes_a_title_this_client_wrote() {
        let state = ClientState::test_new();
        let mut output = Vec::new();
        state
            .host_modes
            .reset_window_title(&mut output)
            .expect("write to a Vec");
        assert!(output.is_empty(), "an untouched host title must stay");

        state
            .host_modes
            .write_window_title(&mut output, Some("agent"))
            .expect("write to a Vec");
        output.clear();
        state
            .host_modes
            .reset_window_title(&mut output)
            .expect("write to a Vec");
        assert_eq!(output, b"\x1b]0;shepr\x07");

        output.clear();
        state
            .host_modes
            .reset_window_title(&mut output)
            .expect("write to a Vec");
        assert!(output.is_empty(), "one reset per written title");
    }
}
