use super::{ClientLoop, ClientLoopAction};
use crate::endpoint::{Admission, SnapshotDirty};
use crate::errors::LoopExit;
use crate::shell_runtime::{ShellInputDisposition, finish_client_shell_input};
use crate::{endpoint, shell};
use shepr_surface::decode::{DecodedClientServerMessage, DecodedWireServerMessage};
use std::io;

impl ClientLoop {
    /// Applies one inbound message to the host and the shell. The hub admits it first: the
    /// generation check and the presentation gate precede every message effect, including
    /// endpoint requests to change host modes or write the clipboard, and move evidence
    /// never reaches this point.
    pub(crate) fn handle_server_message(
        &mut self,
        endpoint_id: &endpoint::ClientEndpointId,
        generation: shepr_protocol::ConnectionGeneration,
        message: Box<DecodedClientServerMessage>,
        now: std::time::Instant,
    ) -> Result<ClientLoopAction, LoopExit> {
        let Self { hub, state, .. } = self;
        let (message, role) = match hub.admit(&mut state.shell, endpoint_id, generation, message) {
            Admission::Consumed => return Ok(ClientLoopAction::NextEvent),
            Admission::Present { message, role } => (*message, role),
        };
        let message = match message {
            DecodedClientServerMessage::Wire(message) => message,
            DecodedClientServerMessage::PaneSurfacePatch(patch) => {
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
                        // The shell repeats the shared admission rule against its presented
                        // baseline. A rejection means the shell and reader no longer share the
                        // baseline the connection decoded, so reconnect for a fresh full one.
                        shepr_platform::structured_log!(
                            ERROR, event = surface.patch, outcome = Refused,
                            endpoint = %endpoint_id,
                            %generation,
                            error = ?reason,
                            "client shell rejected a pane surface patch; failing its connection"
                        );
                        hub.fail(
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
                state.shell.receive_pane_surface_from(surface, generation);
                state.mark_pane_dirty();
            }
            DecodedWireServerMessage::ServerShutdown { reason } => {
                if hub.ends_client_for(endpoint_id) {
                    return Err(LoopExit::ServerShutdown { reason });
                }
                hub.fail(
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
                let completed =
                    hub.complete_command(endpoint_id, generation, &boot_id, &request_id, result);
                let Some(completed) = completed else {
                    return Ok(ClientLoopAction::NextEvent);
                };
                // The whole outcome goes through `finish_client_shell_input`: a
                // copy-mode response replays keys queued while it was in flight,
                // and those can carry pane input, a resize or a detach. It also
                // releases the next queued command in this endpoint's lane.
                let outcome = state.shell.answer_request(
                    &completed.boot_id,
                    &completed.request_id,
                    completed.result,
                    now,
                );
                if finish_client_shell_input(state, outcome, hub, now)?
                    == ShellInputDisposition::Detach
                {
                    return Ok(ClientLoopAction::Detach);
                }
            }
            DecodedWireServerMessage::Clipboard { data } => {
                // Pane programs can issue OSC 52 repeatedly. Keep helper waits
                // and terminal output off this shared event loop; a newer
                // pending copy replaces the older one in the worker.
                if let Some(worker) = &state.clipboard_write_worker {
                    worker.submit(data);
                }
            }
            DecodedWireServerMessage::MouseCapture { mode } => {
                state.host_modes.set_mouse_endpoint_request(mode);
                let result = state
                    .host_modes
                    .apply_mouse(&mut state.output_writer, state.reported_geometry.cell());
                state.record_host_mode_write("endpoint mouse capture", result)?;
            }
            DecodedWireServerMessage::ClientShellKeyboardReportAll { enabled } => {
                let shell_requests_report_all = state.shell.host_keyboard_report_all_requested();
                state.host_modes.set_pane_keyboard_report_all(enabled);
                let result = state.host_modes.sync_shell_keyboard_report_all(
                    &mut state.output_writer,
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
                match snapshot.session_save_status {
                    shepr_protocol::SessionSaveStatus::Ready => {}
                    shepr_protocol::SessionSaveStatus::Stopped => {
                        state
                            .shell
                            .receive_session_saves_stopped(endpoint_id, &snapshot.boot_id);
                    }
                    shepr_protocol::SessionSaveStatus::BlockedOnBackup => {
                        state.shell.receive_session_saves_blocked_on_backup(
                            endpoint_id,
                            &snapshot.boot_id,
                        );
                    }
                }
                match hub.install_snapshot(&mut state.shell, endpoint_id, snapshot, role) {
                    Some(SnapshotDirty::Pane) => state.mark_pane_dirty(),
                    Some(SnapshotDirty::Chrome) => state.mark_chrome_dirty(),
                    None => {}
                }
            }
        }
        Ok(ClientLoopAction::NextEvent)
    }
}
