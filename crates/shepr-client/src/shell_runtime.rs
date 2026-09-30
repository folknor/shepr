use super::*;

pub(super) fn dispatch_client_shell_actions(
    actions: Vec<shell::ClientShellAction>,
    endpoint_commands: &mut endpoint::commands::EndpointCommands,
    endpoints: &mut endpoint::EndpointRegistry,
    presentation: &Presentation,
    output_writer: &mut impl io::Write,
    prefers_osc52_clipboard: bool,
    shell: &mut shell::ClientShellState,
    scheduled_activation: &mut Option<ClientLoopEvent>,
    now: std::time::Instant,
) -> bool {
    let mut repaint = false;
    for action in actions {
        match action {
            shell::ClientShellAction::Endpoint {
                endpoint_id,
                boot_id,
                request,
            } => {
                if let Some(connection) = endpoints.connection(&endpoint_id).filter(|_| {
                    endpoints.active_id() == &endpoint_id
                        && active_endpoint_owns_presentation(presentation, endpoints)
                }) {
                    endpoint_commands.enqueue(
                        endpoint_id,
                        connection.generation.get(),
                        boot_id,
                        request,
                    );
                } else {
                    // This action has not entered the endpoint send queue, so its outcome is
                    // known locally and must not be presented as an interrupted server action.
                    repaint |= shell.cancel_unsent_endpoint_request(&request.id);
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
            } => {
                *scheduled_activation = Some(ClientLoopEvent::ActivateEndpoint {
                    endpoint_id,
                    target,
                    force: false,
                });
            }
        }
    }
    // A source-off-first endpoint activation leaves the registry's committed identity pointing
    // at a deliberately surface-inactive source. Do not drain its retained queue into a server
    // that must reject it; the handoff's commit resumes the committed owner's lane.
    if active_endpoint_owns_presentation(presentation, endpoints) {
        let active_endpoint = endpoints.active_id().clone();
        let cancelled = endpoint_commands.send_next(&active_endpoint, endpoints, now);
        for request_id in cancelled {
            repaint |= shell.cancel_endpoint_request(&request_id);
        }
    }
    repaint
}

pub(super) fn client_shell_resize_message(
    shell: &shell::ClientShellState,
    cols: u16,
    rows: u16,
    cell_width_px: u32,
    cell_height_px: u32,
    pixel_mouse: bool,
) -> ClientMessage {
    let (cell_width_px, cell_height_px, pixel_mouse) =
        super::terminal_geometry::bounded_cell_geometry(cell_width_px, cell_height_px, pixel_mouse);
    let size = shell.surface_size(cols, rows);
    ClientMessage::ClientShellResize {
        geometry: shepr_protocol::TerminalGeometry::new(
            size.cols,
            size.rows,
            cell_width_px,
            cell_height_px,
            pixel_mouse,
        ),
    }
}

/// The geometry every endpoint is asked to render: the host size under the client's own layout.
/// Shell chrome belongs to the client, so all endpoints lay out the same surface.
fn handoff_geometry(state: &ClientState) -> shepr_protocol::TerminalGeometry {
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

/// Brings the handoff in flight to the current host size, rolling it back when the resize cannot
/// be sent. Returns false when no handoff is in flight.
pub(super) fn resize_handoff(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    now: std::time::Instant,
) -> bool {
    if state.presentation.handoff().is_none() {
        return false;
    }
    let geometry = handoff_geometry(state);
    let resized = state
        .presentation
        .handoff_mut()
        .map(|activation| activation.update_resize_at(geometry, endpoints, now));
    if let Some(Err(error)) = resized {
        rollback_endpoint_activation(state, endpoints, &error, false, now);
    }
    true
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

fn install_pending_activation(
    state: &mut ClientState,
    endpoint_commands: &mut endpoint::commands::EndpointCommands,
    next_surface_serial: &mut u64,
    activation: Box<endpoint::PendingEndpointActivation>,
) {
    let retired = activation
        .source_command_lane()
        .map_or_default(|source| endpoint_commands.retire_lane(source));
    for request_id in retired {
        state.shell.cancel_endpoint_request(&request_id);
    }
    *next_surface_serial = next_surface_serial.saturating_add(1);
    state.presentation = Presentation::Handoff(activation);
}

fn local_activation_metadata_ready(
    state: &ClientState,
    endpoints: &endpoint::EndpointRegistry,
) -> bool {
    endpoints
        .connection(&endpoint::ClientEndpointId::Local)
        .is_some_and(|connection| {
            state
                .shell
                .endpoint_snapshot_identity(
                    &endpoint::ClientEndpointId::Local,
                    connection.generation.get(),
                )
                .is_some()
        })
}

pub(super) fn take_ready_local_activation(
    state: &mut ClientState,
    endpoints: &endpoint::EndpointRegistry,
) -> Option<ClientLoopEvent> {
    if !local_activation_metadata_ready(state, endpoints) {
        return None;
    }
    state
        .deferred_local
        .take()
        .map(|intent| ClientLoopEvent::ActivateEndpoint {
            endpoint_id: intent.endpoint_id,
            target: intent.target,
            force: false,
        })
}

pub(super) fn begin_endpoint_activation(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    endpoint_commands: &mut endpoint::commands::EndpointCommands,
    next_surface_serial: &mut u64,
    endpoint_id: endpoint::ClientEndpointId,
    target: Option<shell::ClientEndpointFocusTarget>,
    force: bool,
    now: std::time::Instant,
    scheduled_activation: &mut Option<ClientLoopEvent>,
) -> Result<(), ClientError> {
    state.deferred_local = None;
    if endpoint_id.is_local() && !local_activation_metadata_ready(state, endpoints) {
        state.deferred_local = Some(endpoint::EndpointActivationIntent {
            endpoint_id,
            target,
        });
        state.shell.receive_endpoint_unavailable(
            "Local is reconnecting; selection will resume when it is ready".into(),
        );
        return Ok(());
    }
    let replace_pending = endpoint_id.is_local()
        && state
            .presentation
            .handoff()
            .is_some_and(|activation| !activation.can_retarget(&endpoint_id));
    if !replace_pending && let Some(activation) = state.presentation.handoff_mut() {
        if activation.can_retarget(&endpoint_id) {
            let retarget_error = activation.retarget(target, endpoints).err();
            if let Some(error) = retarget_error {
                rollback_endpoint_activation(state, endpoints, &error, false, now);
            }
        } else {
            // Once rollback starts, even a request for the original target is a new intent.
            // Retain it until restoration finishes; Local can instead abandon this handoff.
            let outcome = activation.supersede_at(endpoint_id, target, endpoints, now);
            if let endpoint::ActivationRollback::Unavailable(message) = outcome {
                state.end_handoff(Presentation::Unavailable);
                present_handoff_unavailable(state, message);
            }
        }
        return Ok(());
    }
    // Only an endpoint that owns the presentation is already active. With no proven owner
    // (`Unavailable`) a pick of the active endpoint re-proves ownership through a handoff.
    let already_active = !replace_pending
        && !force
        && state.presentation.owned()
        && endpoints.active_id() == &endpoint_id
        && endpoints
            .connection(&endpoint_id)
            .is_some_and(|connection| connection.surface_active);
    if already_active {
        if let Some(target) = target {
            let actions = state.shell.focus_endpoint_target(target);
            dispatch_client_shell_actions(
                actions,
                endpoint_commands,
                endpoints,
                &state.presentation,
                &mut state.output_writer,
                state.settings.prefers_osc52_clipboard(),
                &mut state.shell,
                scheduled_activation,
                now,
            );
            if let Some(frame) = state.shell.compose(
                state.reported_geometry.cols(),
                state.reported_geometry.rows(),
            ) {
                state.present_frame(frame);
            }
        }
        return Ok(());
    }
    let geometry = handoff_geometry(state);
    match endpoint::PendingEndpointActivation::prepare(
        &state.shell,
        endpoints,
        &endpoint_id,
        target,
        geometry,
        *next_surface_serial,
        now,
    )
    .and_then(|activation| {
        // Preserve the old transaction if Local fails preflight. After retiring it,
        // all send failures belong to the prepared replacement's rollback path.
        if replace_pending && let Some(previous) = state.presentation.take_handoff() {
            previous.abandon(endpoints);
        }
        activation.start_at(endpoints, now)
    }) {
        Ok(activation) => install_pending_activation(
            state,
            endpoint_commands,
            next_surface_serial,
            Box::new(activation),
        ),
        Err(endpoint::ActivationBeginError::Preflight(error)) => {
            let message = format!("{}: {error}", state.shell.endpoint_label(&endpoint_id));
            state.shell.receive_endpoint_unavailable(message);
        }
        Err(endpoint::ActivationBeginError::Partial { activation, error }) => {
            // A send error is not evidence that its peer did not observe the write. Freeze and
            // retain the lifecycle object before rollback so no source or target output can be
            // projected until one ownership path has been proved again.
            install_pending_activation(state, endpoint_commands, next_surface_serial, activation);
            let error = format!("{}: {error}", state.shell.endpoint_label(&endpoint_id));
            rollback_endpoint_activation(state, endpoints, &error, false, now);
        }
    }
    Ok(())
}

pub(super) fn complete_endpoint_activation(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    endpoint_commands: &mut endpoint::commands::EndpointCommands,
    now: std::time::Instant,
) -> Result<Option<ClientLoopEvent>, ClientError> {
    let host_theme_updates = state.host_theme_updates.clone();
    let completion_result = {
        let Some(activation) = state.presentation.handoff_mut() else {
            return Ok(None);
        };
        activation.complete_with_host_theme_at(
            &mut state.shell,
            endpoints,
            &host_theme_updates,
            now,
        )
    };
    let completion = match completion_result {
        Ok(completion) => completion,
        Err(error) => {
            if !rollback_endpoint_activation(state, endpoints, &error, false, now) {
                state.shell.receive_endpoint_unavailable(error);
            }
            return Ok(None);
        }
    };

    if matches!(
        completion,
        endpoint::ActivationCompletion::AwaitingPresentationSync { .. }
    ) {
        // The coherent target frame replaces the frozen source now (the handoff's phase no
        // longer freezes frames), but pane input stays closed while the handoff owns the
        // presentation, until a second projection epoch has replayed host modes/effects.
        // Written in full: a resize or metadata event may have happened while frozen.
        state.request_repaint();
        let frame = state.shell.compose(
            state.reported_geometry.cols(),
            state.reported_geometry.rows(),
        );
        if let Some(frame) = frame {
            state.present_frame(frame);
        }
        return Ok(None);
    }
    if completion == endpoint::ActivationCompletion::AwaitingPresentationEffects {
        return Ok(None);
    }

    // Owning the presentation is also what opens pane input to the committed endpoint.
    state.end_handoff(Presentation::Owned);
    let successor = match completion {
        endpoint::ActivationCompletion::RestoredSource {
            error,
            successor: next,
            ..
        } => {
            if next.is_none() {
                state.shell.receive_endpoint_unavailable(error);
            }
            next
        }
        // Both awaiting variants returned above; neither carries a successor.
        endpoint::ActivationCompletion::Activated
        | endpoint::ActivationCompletion::AwaitingPresentationSync { .. }
        | endpoint::ActivationCompletion::AwaitingPresentationEffects => None,
    };
    if successor.is_none() {
        let active_endpoint = endpoints.active_id().clone();
        let cancelled = endpoint_commands.send_next(&active_endpoint, endpoints, now);
        for request_id in cancelled {
            state.shell.cancel_endpoint_request(&request_id);
        }
    }
    let frame = state.shell.compose(
        state.reported_geometry.cols(),
        state.reported_geometry.rows(),
    );
    if let Some(frame) = frame {
        state.present_frame(frame);
    }
    // A Local selection made while reconnecting is newer than this transaction's successor.
    if let Some(intent) = successor.filter(|_| state.deferred_local.is_none()) {
        return Ok(Some(ClientLoopEvent::ActivateEndpoint {
            endpoint_id: intent.endpoint_id,
            target: intent.target,
            force: true,
        }));
    }
    Ok(None)
}

/// Reports that the committed endpoint cannot present. Without a handoff in flight nothing owns
/// the presentation any more; a handoff in flight keeps it, since its own rollback decides
/// what owns it next. The notice is client chrome and shows through the freeze.
pub(super) fn present_handoff_unavailable(state: &mut ClientState, message: String) {
    if !state.presentation.handoff_in_flight() {
        state.presentation = Presentation::Unavailable;
    }
    state.shell.receive_endpoint_unavailable(message);
    let frame = state.shell.compose(
        state.reported_geometry.cols(),
        state.reported_geometry.rows(),
    );
    if let Some(frame) = frame {
        state.present_chrome_through_freeze(frame);
    }
}

/// The rollback reason when an endpoint a machine switch involves disconnects mid-switch.
/// `notice` is the same predicate the active-endpoint path shows after the label ("connection
/// was lost; reconnecting", "was removed or re-pointed"), so both read as one
/// sentence about the named machine.
fn handoff_interrupted_notice(label: &str, notice: &str) -> String {
    format!("machine switch interrupted: {label} {notice}")
}

/// Returns whether rollback had to mark the presentation unavailable.
pub(super) fn rollback_endpoint_activation(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    error: &str,
    source_release_rejected: bool,
    now: std::time::Instant,
) -> bool {
    let Some(activation) = state.presentation.handoff_mut() else {
        return false;
    };
    // A pending rollback has moved the handoff into a phase that freezes frames again.
    if let endpoint::ActivationRollback::Unavailable(message) =
        activation.rollback_at(endpoints, error, source_release_rejected, now)
    {
        // No endpoint has been proven safe to present. Keep pane input frozen, but render
        // the client-owned unavailable chrome rather than silently swallowing the error.
        state.end_handoff(Presentation::Unavailable);
        present_handoff_unavailable(state, message);
        return true;
    }
    false
}

pub(super) fn handle_endpoint_disconnect(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    endpoint_commands: &mut endpoint::commands::EndpointCommands,
    supervisors: &mut endpoint::EndpointSupervisors,
    endpoint_id: &endpoint::ClientEndpointId,
    generation: u64,
    now: std::time::Instant,
    notice: &str,
) -> bool {
    supervisors.disconnected(endpoint_id, generation, now);
    let interrupted = handoff_interrupted_notice(state.shell.endpoint_label(endpoint_id), notice);
    if let Some(activation) = state
        .presentation
        .handoff_mut()
        .filter(|activation| activation.involves_endpoint(endpoint_id))
        && let endpoint::ActivationRollback::Unavailable(error) =
            activation.endpoint_disconnected_at(endpoints, endpoint_id, interrupted, now)
    {
        state.end_handoff(Presentation::Unavailable);
        present_handoff_unavailable(state, error);
    }
    let endpoint_was_active = endpoints.active_id() == endpoint_id;
    let cancelled = endpoint_commands.disconnect(endpoint_id);
    for request_id in cancelled {
        state.shell.cancel_endpoint_request(&request_id);
    }
    state.shell.mark_endpoint_disconnected(endpoint_id);
    let unavailable = endpoint_was_active
        .then(|| format!("{} {notice}", state.shell.endpoint_label(endpoint_id)));
    if let Some(message) = unavailable {
        present_handoff_unavailable(state, message);
    } else if let Some(frame) = state.shell.compose(
        state.reported_geometry.cols(),
        state.reported_geometry.rows(),
    ) {
        // A non-active machine going offline only changes its machine-list status.
        state.present_chrome(frame);
    }
    endpoint_was_active
}

/// Whether the registry's active endpoint owns the presentation: nothing short of `Owned` with
/// a live surface counts, so a handoff that ends `Unavailable` is judged failed even when its
/// endpoint's connection kept its surface. This is also the pane input gate: input, endpoint
/// commands and host effects flow only while it holds, so input and presentation cannot
/// disagree. The one exception is inbound host effects during a handoff's validated
/// synchronization phase: `PresentationGate` applies the target's mouse mode, report-all, title
/// and clipboard writes after the target pair has been checked and before the ready fence
/// commits ownership.
pub(super) fn active_endpoint_owns_presentation(
    presentation: &Presentation,
    endpoints: &endpoint::EndpointRegistry,
) -> bool {
    presentation.owned() && endpoints.active_surface_available()
}

/// The handoff the client starts on its own, judged once no handoff work is in flight: to the
/// selected endpoint when it does not own the presentation (its connection has no surface, or
/// nothing owns the presentation) and a handoff could be prepared (metadata for the selected
/// connection and, when the active one keeps a surface, for that too). That covers a reconnected
/// endpoint and re-proving an endpoint whose connection survived an `Unavailable`. A handoff
/// that already failed on this connection is not retried (`EndpointSelectionTracker::suppresses`)
/// until a new connection or an explicit pick, so a failing one cannot loop.
pub(super) fn automatic_activation(
    state: &ClientState,
    endpoints: &endpoint::EndpointRegistry,
    selection: &endpoint::selection::EndpointSelectionTracker,
) -> Option<ClientLoopEvent> {
    let selected = selection.selected_endpoint();
    let connection = endpoints.connection(&selected)?;
    let generation = connection.generation.get();
    let owns_presentation = connection.surface_active && state.presentation.owned();
    if owns_presentation || selection.suppresses(&selected, Some(generation)) {
        return None;
    }
    state
        .shell
        .endpoint_snapshot_identity(&selected, generation)?;
    let active_id = endpoints.active_id();
    let source_ready = endpoints
        .connection(active_id)
        .filter(|active| active.surface_active)
        .is_none_or(|active| {
            state
                .shell
                .endpoint_snapshot_identity(active_id, active.generation.get())
                .is_some()
        });
    source_ready.then_some(ClientLoopEvent::ActivateEndpoint {
        endpoint_id: selected,
        target: None,
        force: false,
    })
}

pub(super) fn install_client_shell_snapshot(
    state: &mut ClientState,
    endpoint_id: &endpoint::ClientEndpointId,
    snapshot: Box<shepr_protocol::ClientShellSnapshot>,
    projection_pending: bool,
    endpoints: &mut endpoint::EndpointRegistry,
) -> Result<(), ClientError> {
    let Some(connection) = endpoints.connection(endpoint_id) else {
        return Ok(());
    };
    let generation = connection.generation.get();
    // While presentation is frozen the projection on screen must not move: a frame composed
    // now may bypass the freeze as client chrome (below), and that is only sound if its
    // snapshot and pane surface are the ones frozen. Metadata is cached instead and projected
    // when the next handoff commits.
    let project_snapshot = !projection_pending
        && !state.presentation.frames_frozen()
        && endpoints.active_id() == endpoint_id
        && connection.surface_active;
    let shell = &mut state.shell;
    let waits_for_selected_surface = projection_pending
        || (endpoints.active_id() == endpoint_id
            && !project_snapshot
            && shell.has_presented_surface());
    if !waits_for_selected_surface {
        shell.set_endpoint_status(endpoint_id, endpoint::ClientEndpointStatus::Online);
    }
    if project_snapshot {
        shell.set_endpoint_snapshot_for_generation(endpoint_id, generation, snapshot);
    } else {
        shell.cache_endpoint_snapshot_for_generation(endpoint_id, generation, snapshot);
    }
    // Snapshot installation updates endpoint data and keybindings, not the shell layout. Layout
    // changes and host resizes each send geometry through their own explicit paths.
    let composed = shell.compose(
        state.reported_geometry.cols(),
        state.reported_geometry.rows(),
    );
    if let Some(frame) = composed {
        // Not inverted, though it reads that way. A snapshot that belongs to an in-flight
        // handoff (`projection_pending`) is the target's (or the restoring source's) metadata:
        // the source frame stays authoritative until the handoff commits, so this frame obeys
        // the freeze. Any other snapshot only moves client chrome (machine list, statuses),
        // because a frozen projection is not advanced (see `project_snapshot`) and frozen pane
        // surfaces are not taken (the `PaneSurface` and patch arms in the client loop); it can
        // pass the freeze without exposing pane output.
        if projection_pending {
            state.present_frame(frame);
        } else {
            state.present_chrome_through_freeze(frame);
        }
    }
    Ok(())
}

pub(super) fn finish_client_shell_input(
    state: &mut ClientState,
    outcome: shell::ClientShellInput,
    frame: Option<shepr_protocol::FrameData>,
    endpoints: &mut endpoint::EndpointRegistry,
    endpoint_commands: &mut endpoint::commands::EndpointCommands,
    scheduled_activation: &mut Option<ClientLoopEvent>,
    now: std::time::Instant,
) -> Result<bool, ClientError> {
    if outcome.detach {
        // A failed send is recorded against the endpoint, and the registry's Drop sends
        // Detach again on the way out.
        endpoints.send(&ClientMessage::Detach);
        return Ok(true);
    }
    if outcome.resize && !resize_handoff(state, endpoints, now) {
        let resize = client_shell_resize_message(
            &state.shell,
            state.reported_geometry.cols(),
            state.reported_geometry.rows(),
            state.reported_geometry.cell_width(),
            state.reported_geometry.cell_height(),
            state.reported_geometry.exact,
        );
        // A failed send is recorded against the active endpoint; the client timer
        // applies the reconnect or local failure policy to it.
        endpoints.send(&resize);
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
        &state.presentation,
        &mut state.output_writer,
        state.settings.prefers_osc52_clipboard(),
        &mut state.shell,
        scheduled_activation,
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
    // Pane input and host effects flow only to an endpoint that owns the presentation, so none
    // crosses a handoff or reaches an endpoint while nothing owns it.
    let active_endpoint_online = state.shell.endpoint_is_online(endpoints.active_id())
        && active_endpoint_owns_presentation(&state.presentation, endpoints);
    for request in outcome.requests {
        if let ClientMessage::ClientShellHostTheme { update } = &request {
            state.record_host_theme_update(update);
            if let Some(activation) = state.presentation.handoff_mut() {
                if let Err(error) = activation.update_host_theme_at(update.clone(), endpoints, now)
                {
                    rollback_endpoint_activation(state, endpoints, &error, false, now);
                }
                continue;
            }
        }
        // Host focus belongs to a pending target even when the source has gone offline or has
        // already had its surface revoked. Route it before the ordinary source-online gate.
        if let ClientMessage::ClientShellFocus { focused } = request {
            if let Some(activation) = state.presentation.handoff_mut() {
                if let Err(error) = activation.update_host_focus_at(focused, endpoints, now) {
                    rollback_endpoint_activation(state, endpoints, &error, false, now);
                }
                continue;
            }
            if active_endpoint_online {
                write_to_server(endpoints, &ClientMessage::ClientShellFocus { focused })
                    .map_err(ClientError::ConnectionLost)?;
            }
            continue;
        }
        if !active_endpoint_online {
            continue;
        }
        write_to_server(endpoints, &request).map_err(ClientError::ConnectionLost)?;
    }
    if let Some(frame) = frame {
        // While nothing owns the presentation, input frames pass the freeze so mode changes,
        // overlays and the machine list stay responsive. That is sound because nothing moves
        // the pane projection while frozen (see `install_client_shell_snapshot` and the pane
        // surface arms of the client loop): the pane cells in this frame are the frozen ones.
        state.present_chrome(frame);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_interrupted_machine_switch_names_the_machine_and_reads_as_one_sentence() {
        assert_eq!(
            handoff_interrupted_notice("buildbox", "connection was lost; reconnecting"),
            "machine switch interrupted: buildbox connection was lost; reconnecting"
        );
        assert_eq!(
            handoff_interrupted_notice("buildbox", "was removed or re-pointed"),
            "machine switch interrupted: buildbox was removed or re-pointed"
        );
    }

    struct NullTransport;

    impl endpoint::EndpointTransport for NullTransport {
        fn send(&mut self, _message: &ClientMessage) -> io::Result<()> {
            Ok(())
        }

        fn disconnect(&mut self) {}

        fn flush(&mut self, _deadline: std::time::Instant) -> io::Result<()> {
            Ok(())
        }

        fn take_error(&mut self) -> Option<io::Error> {
            None
        }
    }

    fn snapshot(boot_id: &str) -> Box<shepr_protocol::ClientShellSnapshot> {
        Box::new(shepr_protocol::ClientShellSnapshot {
            boot_id: crate::tests::test_boot_id(boot_id),
            revision: shepr_protocol::ProjectionRevision::new(1),
            focused_workspace_id: None,
            focused_pane_id: None,
            workspaces: Vec::new(),
            panes: Vec::new(),
            agents: Vec::new(),
        })
    }

    fn is_automatic_activation(
        event: Option<ClientLoopEvent>,
        expected: &endpoint::ClientEndpointId,
    ) -> bool {
        matches!(
            event,
            Some(ClientLoopEvent::ActivateEndpoint {
                endpoint_id,
                target: None,
                force: false,
            }) if &endpoint_id == expected
        )
    }

    #[test]
    fn an_unavailable_owner_whose_connection_survived_is_reproved_once_per_connection() {
        let local = endpoint::ClientEndpointId::Local;
        let mut state = ClientState::test_new();
        state
            .shell
            .set_endpoint_snapshot_for_generation(&local, 1, snapshot("local-boot"));
        let mut endpoints = endpoint::EndpointRegistry::new(NullTransport, 1);
        let mut selection = endpoint::selection::EndpointSelectionTracker::new(Vec::new());

        // Nothing to do while Local owns the presentation.
        assert!(active_endpoint_owns_presentation(
            &state.presentation,
            &endpoints
        ));
        assert!(automatic_activation(&state, &endpoints, &selection).is_none());

        // Nothing owns the presentation although Local kept its surface: re-prove it.
        state.presentation = Presentation::Unavailable;
        assert!(!active_endpoint_owns_presentation(
            &state.presentation,
            &endpoints
        ));
        assert!(is_automatic_activation(
            automatic_activation(&state, &endpoints, &selection),
            &local
        ));

        // That handoff fails, leaving the presentation unavailable. The selection judges it
        // failed although the connection kept its surface, and it is not retried.
        assert!(selection.begin(&local, Some(1)));
        assert_eq!(
            selection.settle(
                false,
                endpoints.active_id(),
                active_endpoint_owns_presentation(&state.presentation, &endpoints)
            ),
            endpoint::selection::SelectionOutcome::Reverted
        );
        assert!(
            automatic_activation(&state, &endpoints, &selection).is_none(),
            "a re-proof that fails again must not loop"
        );

        // A new connection may be tried once it has metadata.
        endpoints.insert(
            local.clone(),
            NullTransport,
            2,
            true,
            std::time::Instant::now(),
        );
        assert!(automatic_activation(&state, &endpoints, &selection).is_none());
        state
            .shell
            .set_endpoint_snapshot_for_generation(&local, 2, snapshot("local-boot"));
        assert!(is_automatic_activation(
            automatic_activation(&state, &endpoints, &selection),
            &local
        ));

        // A connection without a surface is activated whatever owns the presentation.
        state.presentation = Presentation::Owned;
        endpoints.set_surface_active(&local, false);
        assert!(is_automatic_activation(
            automatic_activation(&state, &endpoints, &selection),
            &local
        ));
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
