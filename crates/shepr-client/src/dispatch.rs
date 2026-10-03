use crate::client_loop::{ClientLoop, ClientLoopAction};
use crate::clipboard_forwarding::forward_clipboard;
use crate::errors::LoopExit;
use crate::shell_runtime::{
    ShellInputDisposition, finish_client_shell_input, install_client_shell_snapshot,
};
use crate::{endpoint, shell, state};
use shepr_surface::decode::{DecodedClientServerMessage, DecodedWireServerMessage};
use std::io;
use tracing::warn;

impl ClientLoop {
    // Generation admission and the presentation gate precede every message effect,
    // including endpoint requests to change host modes or write the clipboard.
    pub(crate) fn handle_server_message(
        &mut self,
        endpoint_id: &endpoint::ClientEndpointId,
        generation: u64,
        message: Box<DecodedClientServerMessage>,
        now: std::time::Instant,
    ) -> Result<ClientLoopAction, LoopExit> {
        let Self {
            write_stream,
            endpoint_commands,
            state,
            local_failure_policy,
            ..
        } = self;
        if !write_stream.accepts(endpoint_id, generation) {
            return Ok(ClientLoopAction::NextEvent);
        }
        let role = state.shell.endpoints.choice.role(endpoint_id);
        let move_response =
            match message.as_ref() {
                DecodedClientServerMessage::Wire(
                    DecodedWireServerMessage::ClientShellEndpointResponse {
                        boot_id,
                        request_id,
                        ..
                    },
                ) => state.shell.endpoints.choice.preparing().is_some_and(|p| {
                    p.accepts_response(endpoint_id, generation, boot_id, request_id)
                }),
                _ => false,
            };
        let presentation_decision =
            endpoint::PresentationGate::new(role, move_response).decide(message.as_ref());
        if presentation_decision == endpoint::PresentationDecision::Drop {
            return Ok(ClientLoopAction::NextEvent);
        }
        let message = match *message {
            DecodedClientServerMessage::Wire(message) => message,
            DecodedClientServerMessage::PaneSurfacePatch(patch) => {
                if presentation_decision.buffers() {
                    if let Some(pending) = state.shell.endpoints.choice.preparing_mut() {
                        pending.receive_patch(endpoint_id, generation, &patch);
                    }
                    return Ok(ClientLoopAction::NextEvent);
                }
                let outcome = state
                    .shell
                    .apply_pane_surface_patch_from(&patch, generation);
                match outcome {
                    shell::ClientPaneSurfacePatchOutcome::Applied(
                        shell::PatchPresentation::Rows(composed),
                    ) => state.queue_surface_patch(composed),
                    shell::ClientPaneSurfacePatchOutcome::Applied(
                        shell::PatchPresentation::Compose,
                    ) => state.mark_pane_dirty(),
                    shell::ClientPaneSurfacePatchOutcome::Applied(
                        shell::PatchPresentation::Held,
                    ) => {}
                    shell::ClientPaneSurfacePatchOutcome::Rejected(reason) => {
                        // The patch does not follow the shell's baseline, which mirrors the
                        // reader's. Either the two disagree about a baseline both derive from
                        // the same wire, or the server sent a patch the decoder accepts and the
                        // shell does not (a pane geometry change, or a row outside every
                        // patched pane; the decoder checks neither). Both are bugs, and
                        // reconnecting for a fresh full surface baseline is the one response.
                        tracing::error!(
                            endpoint = %endpoint_id,
                            generation,
                            ?reason,
                            "client shell rejected a pane surface patch; failing its connection"
                        );
                        write_stream.fail(
                            endpoint_id,
                            &io::Error::new(
                                io::ErrorKind::InvalidData,
                                shepr_launch::EndpointFailure::incompatible(
                                    "client shell rejected a pane surface patch",
                                ),
                            ),
                        );
                    }
                }
                return Ok(ClientLoopAction::NextEvent);
            }
        };
        match message {
            DecodedWireServerMessage::PaneSurface(surface) => {
                if presentation_decision.buffers() {
                    if let Some(pending) = state.shell.endpoints.choice.preparing_mut() {
                        pending.receive_surface(endpoint_id, generation, surface);
                    }
                    return Ok(ClientLoopAction::NextEvent);
                }
                state.shell.receive_pane_surface_from(surface, generation);
                state.mark_pane_dirty();
            }
            DecodedWireServerMessage::ServerShutdown { reason } => {
                if local_failure_policy.ends_client_for(endpoint_id.policy()) {
                    return Err(LoopExit::ServerShutdown { reason });
                }
                write_stream.fail(
                    endpoint_id,
                    &io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        shepr_launch::EndpointFailure::server_shutdown(reason),
                    ),
                );
            }
            DecodedWireServerMessage::ClientShellError { kind } => {
                if state.shell.receive_server_notice(&kind) {
                    // The error banner is chrome; it must show while nothing is shown,
                    // like machine statuses do.
                    state.mark_chrome_dirty();
                }
            }
            DecodedWireServerMessage::ClientShellEndpointResponse {
                boot_id,
                request_id,
                result,
            } => {
                if presentation_decision.buffers() {
                    if let Some(pending) = state.shell.endpoints.choice.preparing_mut() {
                        pending.receive_response(
                            endpoint_id,
                            generation,
                            &boot_id,
                            &request_id,
                            result,
                        );
                    }
                    return Ok(ClientLoopAction::NextEvent);
                }
                let completed = endpoint_commands.receive_response(
                    endpoint_id,
                    generation,
                    &boot_id,
                    &request_id,
                    result,
                );
                let Some(completed) = completed else {
                    return Ok(ClientLoopAction::NextEvent);
                };
                // The whole outcome goes through `finish_client_shell_input`: a
                // copy-mode response replays keys queued while it was in flight,
                // and those can carry pane input, a resize or a detach. It also
                // releases the next queued command in this endpoint's lane.
                let shell = &mut state.shell;
                let outcome = if shell.endpoint_is_active(&completed.endpoint_id) {
                    shell.answer_request(
                        &completed.boot_id,
                        &completed.request_id,
                        completed.result,
                        now,
                    )
                } else {
                    shell::ClientShellInput {
                        repaint: shell
                            .drop_request(&completed.request_id, shell::DropReason::Interrupted)
                            .is_needed(),
                        ..Default::default()
                    }
                };
                if finish_client_shell_input(state, outcome, write_stream, endpoint_commands, now)?
                    == ShellInputDisposition::Detach
                {
                    return Ok(ClientLoopAction::Exit);
                }
            }
            DecodedWireServerMessage::Clipboard { data } => {
                // write_clipboard_bytes flushes its own OSC 52 fallback, so no flush is
                // needed here. Once per user copy, so a warn cannot flood; only the
                // payload length is logged because the bytes are the user's selection.
                if let Err(error) = forward_clipboard(
                    &data,
                    state.settings.prefers_osc52_clipboard(),
                    &mut state.output_writer,
                ) {
                    warn!(
                        endpoint = %endpoint_id,
                        generation,
                        bytes = data.len(),
                        %error,
                        "clipboard copy from the server did not reach the host clipboard"
                    );
                }
            }
            DecodedWireServerMessage::WindowTitle { title } => {
                // `None` is deliberate from the server (an API title was
                // cleared, or every template token resolved empty) and
                // resets to Shepr's default. A disabled `ui.window_title`
                // never reaches here: the server sends nothing at all.
                // A lost title write is cosmetic and the next title change retries it;
                // logged once per cause because titles can change with every agent state.
                let written = state
                    .host_modes
                    .write_window_title(&mut state.output_writer, title.as_deref());
                state.title_write_failure.observe(
                    state::HostWritePurpose::Title,
                    "window title",
                    &written,
                    None,
                );
            }
            DecodedWireServerMessage::MouseCapture {
                enabled,
                sgr_pixels,
            } => {
                state
                    .host_modes
                    .set_mouse_endpoint_request(enabled, sgr_pixels);
                let result = state.host_modes.apply_mouse(
                    &mut state.output_writer,
                    state.reported_geometry.exact(),
                    false,
                );
                state.record_host_mode_write("endpoint mouse capture", result)?;
            }
            DecodedWireServerMessage::ClientShellKeyboardReportAll { enabled } => {
                let shell_requests_report_all = state.shell.host_keyboard_report_all_requested();
                let result = state.host_modes.set_pane_keyboard_report_all(
                    &mut state.output_writer,
                    enabled,
                    shell_requests_report_all,
                );
                state.record_host_mode_write("keyboard report-all request", result)?;
            }
            DecodedWireServerMessage::HealthPong => {
                return Ok(ClientLoopAction::NextEvent);
            }
            DecodedWireServerMessage::EndpointSnapshot(snapshot) => {
                if let Some(kind) = snapshot.restore_notice.as_ref() {
                    state
                        .shell
                        .receive_restore_notice(endpoint_id, &snapshot.boot_id, kind);
                }
                if snapshot.session_saves_stopped {
                    state
                        .shell
                        .receive_session_saves_stopped(endpoint_id, &snapshot.boot_id);
                }
                if presentation_decision.buffers()
                    && let Some(pending) = state.shell.endpoints.choice.preparing_mut()
                {
                    pending.receive_snapshot(endpoint_id, generation, &snapshot);
                }
                install_client_shell_snapshot(state, endpoint_id, snapshot, role, write_stream);
                write_stream.mark_ready(endpoint_id, generation);
            }
        }
        Ok(ClientLoopAction::NextEvent)
    }
}
