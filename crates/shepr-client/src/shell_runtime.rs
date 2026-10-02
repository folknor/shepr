use super::*;

pub(super) fn cancel_endpoint_commands(
    shell: &mut shell::ClientShellState,
    cancelled: endpoint::commands::EndpointCommandCancellation,
) -> bool {
    let mut repaint = false;
    for request_id in cancelled.unsent {
        repaint |= shell.drop_request(&request_id, shell::DropReason::Unsent);
    }
    for request_id in cancelled.possibly_sent {
        repaint |= shell.drop_request(&request_id, shell::DropReason::Interrupted);
    }
    repaint
}

/// Settles every in-flight endpoint command whose deadline has passed, each exactly once.
/// `expire` takes the command out of its lane, so whatever is not answered here is never
/// reported by a later lane disconnect. A command whose endpoint is no longer active, or whose
/// connection is gone, is dropped as interrupted: the timer tick that expires it can first
/// record a failed health check, which removes the connection before the next reconcile
/// disconnects the lane. Only a command still on its own connection to the active endpoint
/// is answered with its timeout.
pub(super) fn settle_expired_endpoint_commands(
    endpoint_commands: &mut endpoint::commands::EndpointCommands,
    endpoints: &endpoint::EndpointRegistry,
    shell: &mut shell::ClientShellState,
    now: std::time::Instant,
) -> shell::ClientShellInput {
    let mut outcome = shell::ClientShellInput::default();
    for expired in endpoint_commands.expire(now) {
        if !endpoints.accepts(&expired.endpoint_id, expired.generation)
            || !shell.endpoint_is_active(&expired.endpoint_id)
        {
            outcome.repaint |=
                shell.drop_request(&expired.request_id, shell::DropReason::Interrupted);
            continue;
        }
        outcome.merge(shell.answer_request(
            &expired.boot_id,
            &expired.request_id,
            expired.result,
            now,
        ));
    }
    outcome
}

/// Where pane input and endpoint commands go: the shown endpoint, while its connection exists
/// and is viewed. During a move that is the source, which stays live until the commit.
pub(super) fn input_endpoint<'a>(
    choice: &'a endpoint::EndpointChoice,
    endpoints: &endpoint::EndpointRegistry,
) -> Option<&'a endpoint::ClientEndpointId> {
    choice.shown().filter(|id| endpoints.viewed(id))
}

pub(super) fn dispatch_client_shell_actions(
    actions: Vec<shell::ClientShellAction>,
    endpoint_commands: &mut endpoint::commands::EndpointCommands,
    endpoints: &mut endpoint::EndpointRegistry,
    choice: &mut endpoint::EndpointChoice,
    output_writer: &mut impl io::Write,
    prefers_osc52_clipboard: bool,
    shell: &mut shell::ClientShellState,
    now: std::time::Instant,
) -> bool {
    let mut repaint = false;
    let mut actions = std::collections::VecDeque::from(actions);
    while let Some(action) = actions.pop_front() {
        match action {
            shell::ClientShellAction::Endpoint {
                endpoint_id,
                boot_id,
                request,
            } => {
                if input_endpoint(choice, endpoints) == Some(&endpoint_id)
                    && let Some(connection) = endpoints.connection(&endpoint_id)
                {
                    endpoint_commands.enqueue(
                        endpoint_id,
                        connection.generation.get(),
                        boot_id,
                        request,
                    );
                } else {
                    // This action has not entered the endpoint send queue, so its outcome is
                    // known locally and must not be presented as an interrupted server action.
                    repaint |= shell.drop_request(&request.id, shell::DropReason::Unsent);
                }
            }
            shell::ClientShellAction::ClipboardWrite(bytes) => {
                // Once per user copy, so a warn cannot flood; only the length is
                // logged because the bytes are the user's selection.
                if let Err(error) = shepr_termio::host_term::title::write_clipboard_bytes(
                    &bytes,
                    prefers_osc52_clipboard,
                    output_writer,
                ) {
                    warn!(
                        bytes = bytes.len(),
                        %error,
                        "clipboard copy did not reach the host clipboard"
                    );
                }
            }
            shell::ClientShellAction::ActivateEndpoint {
                endpoint_id,
                target,
            } => match choice.select(endpoint_id.clone(), target) {
                endpoint::Selection::Unchanged => {}
                endpoint::Selection::FocusShown(target) => {
                    actions.extend(shell.focus_endpoint_target(target));
                    repaint = true;
                }
                endpoint::Selection::Moving => {
                    let connection = endpoints.connection(&endpoint_id);
                    let metadata_ready = connection.is_some_and(|connection| {
                        shell
                            .endpoint_snapshot_identity(&endpoint_id, connection.generation.get())
                            .is_some()
                    });
                    // A machine without a connection while something is shown is abandoned by
                    // the next reconcile with its own "is not ready" notice; promising that
                    // the selection resumes would contradict it.
                    let abandoned =
                        connection.is_none() && !endpoint_id.is_local() && choice.shown().is_some();
                    if !metadata_ready && !abandoned {
                        let notice = waiting_notice(
                            endpoint_id.display_label(),
                            shell.endpoint_status(&endpoint_id),
                        );
                        repaint |= shell.receive_endpoint_unavailable(notice);
                    }
                }
            },
        }
    }
    if let Some(shown) = input_endpoint(choice, endpoints) {
        let cancelled = endpoint_commands.send_next(shown, endpoints, now);
        repaint |= cancel_endpoint_commands(shell, cancelled);
    }
    repaint
}

/// The notice for a pick that has to wait for its endpoint's connection or metadata. It names
/// the current status, so an attention diagnostic does not read like a promise that waiting
/// will repair it.
pub(super) fn waiting_notice(
    label: &str,
    status: Option<endpoint::ClientEndpointStatus>,
) -> String {
    use endpoint::ClientEndpointStatus::*;
    match status {
        Some(Connecting) => {
            format!("{label} is connecting; selection will resume when it is ready")
        }
        Some(Reconnecting) => {
            format!("{label} is reconnecting; selection will resume when it is ready")
        }
        Some(Attention) => format!("{label} needs attention"),
        _ => format!(
            "{label} is waiting for its workspace snapshot; selection will resume when it is ready"
        ),
    }
}

/// The geometry every endpoint is asked to render: the host size under the client's own layout.
/// Shell chrome belongs to the client, so all endpoints lay out the same surface.
pub(super) fn view_geometry(state: &ClientState) -> shepr_protocol::TerminalGeometry {
    let geometry = &state.reported_geometry;
    let (cell_width_px, cell_height_px, pixel_mouse) =
        super::terminal_geometry::bounded_cell_geometry(
            geometry.cell_width(),
            geometry.cell_height(),
            geometry.exact,
        );
    let size = state.shell.surface_size(geometry.cols(), geometry.rows());
    shepr_protocol::TerminalGeometry::new(
        size.cols,
        size.rows,
        cell_width_px,
        cell_height_px,
        pixel_mouse,
    )
}

/// Sends the current geometry to every viewed connection: the shown endpoint and a target
/// being prepared. A changed geometry also drops the move's recorded surface; an unchanged
/// one keeps it, because the server answers an unchanged resize with no new surface.
pub(super) fn resize_views(state: &mut ClientState, endpoints: &mut endpoint::EndpointRegistry) {
    let geometry = view_geometry(state);
    if let Some(preparing) = state.choice.preparing_mut() {
        preparing.update_geometry(geometry);
    }
    endpoints.send_viewed(&ClientMessage::ClientShellResize { geometry });
}

pub(super) fn sync_client_shell_keyboard_report_all(
    state: &mut ClientState,
) -> Result<(), ClientError> {
    state
        .host_modes
        .sync_shell_keyboard_report_all(
            &mut state.output_writer,
            state.shell.host_keyboard_report_all_requested(),
        )
        .map_err(ClientError::HostTerminal)
}

/// Drops the host terminal effects a lost or retired endpoint asked for. Every step runs even
/// after one fails, so a failed mouse reset still clears report-all and the title; the first
/// failure is then returned as a host terminal error, like every other host mode write on the
/// client loop.
pub(super) fn clear_endpoint_host_effects(state: &mut ClientState) -> Result<(), ClientError> {
    state.host_modes.clear_mouse_endpoint_request();
    let mouse = state.host_modes.apply_mouse(
        &mut state.output_writer,
        state.reported_geometry.exact,
        false,
    );
    let shell_requests_report_all = state.shell.host_keyboard_report_all_requested();
    let report_all = state.host_modes.set_pane_keyboard_report_all(
        &mut state.output_writer,
        false,
        shell_requests_report_all,
    );
    let title = state
        .host_modes
        .reset_window_title(&mut state.output_writer);
    mouse
        .and(report_all)
        .and(title)
        .map_err(ClientError::HostTerminal)
}

pub(super) fn install_client_shell_snapshot(
    state: &mut ClientState,
    endpoint_id: &endpoint::ClientEndpointId,
    snapshot: Box<shepr_protocol::ClientShellSnapshot>,
    role: endpoint::ConnectionRole,
    endpoints: &mut endpoint::EndpointRegistry,
) -> Result<(), ClientError> {
    let Some(connection) = endpoints.connection(endpoint_id) else {
        return Ok(());
    };
    let generation = connection.generation.get();
    state
        .shell
        .set_endpoint_status(endpoint_id, endpoint::ClientEndpointStatus::Online);
    // Only the shown endpoint's snapshot moves the projection. Any other one (a target being
    // prepared included) is cached for its commit, so the chrome frame below shows the same
    // projection and pane cells as before and needs no gate. Snapshot application only moves
    // Copy and Terminal modes; neither asks the host for report-all keys.
    if role == endpoint::ConnectionRole::Shown {
        state
            .shell
            .set_endpoint_snapshot_for_generation(endpoint_id, generation, snapshot);
    } else {
        state
            .shell
            .cache_endpoint_snapshot_for_generation(endpoint_id, generation, snapshot);
    }
    if let Some(frame) = state.shell.compose(
        state.reported_geometry.cols(),
        state.reported_geometry.rows(),
    ) {
        state.present_chrome(frame);
    }
    Ok(())
}

pub(super) fn finish_client_shell_input(
    state: &mut ClientState,
    outcome: shell::ClientShellInput,
    frame: Option<shepr_protocol::FrameData>,
    endpoints: &mut endpoint::EndpointRegistry,
    endpoint_commands: &mut endpoint::commands::EndpointCommands,
    now: std::time::Instant,
) -> Result<bool, ClientError> {
    if outcome.detach {
        // A failed send is recorded against the endpoint. The registry remembers a sent
        // Detach, so its Drop on the way out only flushes this connection.
        if let Some(shown) = state.choice.shown() {
            endpoints.send_to(shown, &ClientMessage::Detach);
        }
        return Ok(true);
    }
    if outcome.resize {
        resize_views(state, endpoints);
    }
    if outcome.full_redraw {
        // Discard the blit baseline so the frame below is written in full; the
        // host may have lost or garbled what we last drew while unfocused.
        state.request_repaint();
    }
    if outcome.query_host_appearance {
        query_host_terminal_appearance(&mut state.output_writer);
    }
    if outcome.query_host_theme {
        query_host_terminal_theme(&mut state.output_writer);
    }
    sync_client_shell_keyboard_report_all(state)?;
    let dispatch_repaint = dispatch_client_shell_actions(
        outcome.actions,
        endpoint_commands,
        endpoints,
        &mut state.choice,
        &mut state.output_writer,
        state.settings.prefers_osc52_clipboard(),
        &mut state.shell,
        now,
    );
    let frame = if dispatch_repaint {
        state.shell.compose(
            state.reported_geometry.cols(),
            state.reported_geometry.rows(),
        )
    } else {
        frame
    };
    for request in outcome.requests {
        match &request {
            ClientMessage::ClientShellHostTheme { update } => {
                state.record_host_theme_update(update);
                endpoints.send_viewed(&request);
            }
            ClientMessage::ClientShellResize { .. } => {
                resize_views(state, endpoints);
            }
            // Pane input and host focus reach only the shown endpoint. A target learns of host
            // focus at its commit, and a released endpoint was sent focus-loss with its release.
            _ => {
                if let Some(shown) = input_endpoint(&state.choice, endpoints) {
                    endpoints.send_to(shown, &request);
                }
            }
        }
    }
    if let Some(frame) = frame {
        state.present_chrome(frame);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_notice_names_the_endpoint_and_its_current_status() {
        assert_eq!(
            waiting_notice("Local", Some(endpoint::ClientEndpointStatus::Attention)),
            "Local needs attention"
        );
        assert_eq!(
            waiting_notice("build", Some(endpoint::ClientEndpointStatus::Reconnecting)),
            "build is reconnecting; selection will resume when it is ready"
        );
        assert_eq!(
            waiting_notice("build", Some(endpoint::ClientEndpointStatus::Online)),
            "build is waiting for its workspace snapshot; selection will resume when it is ready"
        );
    }

    #[test]
    fn transport_failures_map_to_fixed_disconnect_notices() {
        assert_eq!(
            shepr_remote::EndpointFailure::from_error(&io::Error::from(
                io::ErrorKind::UnexpectedEof
            ))
            .disconnect_notice(),
            "connection was lost; reconnecting"
        );
        assert_eq!(
            shepr_remote::EndpointFailure::from_error(&io::Error::from(io::ErrorKind::TimedOut))
                .disconnect_notice(),
            "connection timed out; reconnecting"
        );
        assert_eq!(
            shepr_remote::EndpointFailure::from_error(&io::Error::from(io::ErrorKind::InvalidData))
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
