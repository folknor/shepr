use super::*;

pub(super) fn dispatch_client_shell_actions(
    actions: Vec<shell::ClientShellAction>,
    endpoint_commands: &mut endpoint::commands::EndpointCommands,
    endpoints: &mut endpoint::EndpointRegistry,
    output_writer: &mut impl io::Write,
    prefers_osc52_clipboard: bool,
    mut shell: Option<&mut shell::ClientShellState>,
    scheduled_activation: &mut Option<ClientLoopEvent>,
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
                    endpoints.active_id() == &endpoint_id && endpoints.active_surface_available()
                }) {
                    endpoint_commands.enqueue(
                        endpoint_id,
                        connection.generation.get(),
                        boot_id,
                        request,
                    );
                } else if let Some(shell) = shell.as_deref_mut() {
                    repaint |= shell.cancel_endpoint_request(&request.id);
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
    // that must reject it; completion below resumes the committed owner's lane.
    if endpoints.active_surface_available() {
        let active_endpoint = endpoints.active_id().clone();
        let cancelled = endpoint_commands.send_next(&active_endpoint, endpoints);
        if let Some(shell) = shell {
            for request_id in cancelled {
                repaint |= shell.cancel_endpoint_request(&request_id);
            }
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

pub(super) fn sync_client_shell_keyboard_report_all(
    state: &mut ClientState,
) -> Result<(), ClientError> {
    let Some(shell) = state.mode.shell() else {
        return Ok(());
    };
    state
        .host_modes
        .sync_shell_keyboard_report_all(
            &mut state.output_writer,
            shell.host_keyboard_report_all_requested(),
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
        state.mode.is_shell(),
        state.reported_geometry.exact,
        false,
    );
    let shell_requests_report_all = state
        .mode
        .shell()
        .is_some_and(ShellSession::host_keyboard_report_all_requested);
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
    pending: &mut Option<endpoint::PendingEndpointActivation>,
    next_surface_serial: &mut u64,
    activation: endpoint::PendingEndpointActivation,
) {
    let retired = activation
        .source_command_lane()
        .map_or_default(|source| endpoint_commands.retire_lane(source));
    if let Some(shell) = state.mode.shell_mut() {
        for request_id in retired {
            shell.cancel_endpoint_request(&request_id);
        }
    }
    *next_surface_serial = next_surface_serial.saturating_add(1);
    state.freeze_presentation();
    *pending = Some(activation);
}

fn local_activation_metadata_ready(
    state: &ClientState,
    endpoints: &endpoint::EndpointRegistry,
) -> bool {
    endpoints
        .connection(&endpoint::ClientEndpointId::Local)
        .is_some_and(|connection| {
            state.mode.shell().is_some_and(|shell| {
                shell
                    .endpoint_snapshot_identity(
                        &endpoint::ClientEndpointId::Local,
                        connection.generation.get(),
                    )
                    .is_some()
            })
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
        .deferred_local_activation
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
    pending: &mut Option<endpoint::PendingEndpointActivation>,
    next_surface_serial: &mut u64,
    endpoint_id: endpoint::ClientEndpointId,
    target: Option<shell::ClientEndpointFocusTarget>,
    force: bool,
    now: std::time::Instant,
    scheduled_activation: &mut Option<ClientLoopEvent>,
) -> Result<(), ClientError> {
    state.deferred_local_activation = None;
    if endpoint_id.is_local() && !local_activation_metadata_ready(state, endpoints) {
        state.deferred_local_activation = Some(endpoint::EndpointActivationIntent {
            endpoint_id,
            target,
        });
        if let Some(shell) = state.mode.shell_mut() {
            shell.receive_endpoint_unavailable(
                "Local is reconnecting; selection will resume when it is ready".into(),
            );
        }
        return Ok(());
    }
    let replace_pending = endpoint_id.is_local()
        && pending
            .as_ref()
            .is_some_and(|activation| !activation.can_retarget(&endpoint_id));
    if !replace_pending && let Some(activation) = pending.as_mut() {
        if activation.can_retarget(&endpoint_id) {
            let retarget_error = activation.retarget(target, endpoints).err();
            if let Some(error) = retarget_error {
                rollback_endpoint_activation(state, endpoints, pending, &error, false);
            }
        } else {
            // Once rollback starts, even a request for the original target is a new intent.
            // Retain it until restoration finishes; Local can instead abandon this handoff.
            let outcome = activation.supersede(endpoint_id, target, endpoints);
            if let endpoint::ActivationRollback::Unavailable(message) = outcome {
                *pending = None;
                present_handoff_unavailable(state, message);
            }
        }
        return Ok(());
    }
    let already_active = !replace_pending
        && !force
        && endpoints.active_id() == &endpoint_id
        && endpoints
            .connection(&endpoint_id)
            .is_some_and(|connection| connection.surface_active);
    if already_active {
        if let (Some(shell), Some(target)) = (state.mode.shell_mut(), target) {
            let actions = shell.focus_endpoint_target(target);
            let repaint = dispatch_client_shell_actions(
                actions,
                endpoint_commands,
                endpoints,
                &mut state.output_writer,
                state.settings.prefers_osc52_clipboard(),
                Some(shell),
                scheduled_activation,
            );
            if repaint
                && let Some(frame) = shell.compose(
                    state.reported_geometry.cols(),
                    state.reported_geometry.rows(),
                )
            {
                state.present_frame(frame);
            }
        }
        return Ok(());
    }
    let Some(shell) = state.mode.shell() else {
        return Ok(());
    };
    let resize = client_shell_resize_message(
        shell,
        state.reported_geometry.cols(),
        state.reported_geometry.rows(),
        state.reported_geometry.cell_width(),
        state.reported_geometry.cell_height(),
        state.reported_geometry.exact,
    );
    match endpoint::PendingEndpointActivation::prepare(
        shell,
        endpoints,
        &endpoint_id,
        target,
        resize,
        *next_surface_serial,
        now,
    )
    .and_then(|activation| {
        // Preserve the old transaction if Local fails preflight. After retiring it,
        // all send failures belong to the prepared replacement's rollback path.
        if replace_pending && let Some(previous) = pending.take() {
            previous.abandon(endpoints);
        }
        activation.start(endpoints)
    }) {
        Ok(activation) => install_pending_activation(
            state,
            endpoint_commands,
            pending,
            next_surface_serial,
            activation,
        ),
        Err(endpoint::ActivationBeginError::Preflight(error)) => {
            if let Some(shell) = state.mode.shell_mut() {
                shell.receive_endpoint_unavailable(format!(
                    "{}: {error}",
                    shell.endpoint_label(&endpoint_id)
                ));
            }
        }
        Err(endpoint::ActivationBeginError::Partial { activation, error }) => {
            // A send error is not evidence that its peer did not observe the write. Freeze and
            // retain the lifecycle object before rollback so no source or target output can be
            // projected until one ownership path has been proved again.
            install_pending_activation(
                state,
                endpoint_commands,
                pending,
                next_surface_serial,
                *activation,
            );
            rollback_endpoint_activation(
                state,
                endpoints,
                pending,
                &format!(
                    "{}: {error}",
                    state.mode.shell().map_or_else(
                        || format!("{endpoint_id:?}"),
                        |shell| shell.endpoint_label(&endpoint_id).to_owned(),
                    )
                ),
                false,
            );
        }
    }
    Ok(())
}

pub(super) fn complete_endpoint_activation(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    pending: &mut Option<endpoint::PendingEndpointActivation>,
    endpoint_commands: &mut endpoint::commands::EndpointCommands,
) -> Result<Option<ClientLoopEvent>, ClientError> {
    let sync_endpoint = pending
        .as_ref()
        .and_then(endpoint::PendingEndpointActivation::presentation_sync_endpoint)
        .cloned();
    if let Some(endpoint_id) = sync_endpoint.as_ref() {
        state.replay_host_theme(endpoints, endpoint_id);
    }
    let completion = {
        let Some(activation) = pending.as_mut() else {
            return Ok(None);
        };
        let Some(shell) = state.mode.shell_mut() else {
            return Ok(None);
        };
        match activation.complete(shell, endpoints) {
            Ok(completion) => completion,
            Err(error) => {
                shell.receive_endpoint_unavailable(error);
                return Ok(None);
            }
        }
    };

    if matches!(
        completion,
        endpoint::ActivationCompletion::AwaitingPresentationSync { .. }
    ) {
        // The coherent target frame can replace the frozen source now, but the registry keeps
        // pane input disabled until a second projection epoch has replayed host modes/effects.
        state.unfreeze_presentation();
        let frame = state.mode.shell_mut().and_then(|shell| {
            shell.compose(
                state.reported_geometry.cols(),
                state.reported_geometry.rows(),
            )
        });
        if let Some(frame) = frame {
            state.present_frame(frame);
        }
        return Ok(None);
    }
    if completion == endpoint::ActivationCompletion::AwaitingPresentationEffects {
        return Ok(None);
    }

    let requested_surface_size = pending
        .take()
        .map(|activation| activation.requested_surface_size());
    endpoints.unfreeze_input();
    let successor = match completion {
        endpoint::ActivationCompletion::RestoredSource {
            error,
            successor: next,
            ..
        } => {
            if next.is_none()
                && let Some(shell) = state.mode.shell_mut()
            {
                shell.receive_endpoint_unavailable(error);
            }
            next
        }
        // Both awaiting variants returned above; neither carries a successor.
        endpoint::ActivationCompletion::Activated
        | endpoint::ActivationCompletion::AwaitingPresentationSync { .. }
        | endpoint::ActivationCompletion::AwaitingPresentationEffects => None,
    };
    state.unfreeze_presentation();
    if successor.is_none() {
        correct_committed_surface_size(state, endpoints, requested_surface_size);
        let active_endpoint = endpoints.active_id().clone();
        let cancelled = endpoint_commands.send_next(&active_endpoint, endpoints);
        if let Some(shell) = state.mode.shell_mut() {
            for request_id in cancelled {
                shell.cancel_endpoint_request(&request_id);
            }
        }
    }
    let frame = state.mode.shell_mut().and_then(|shell| {
        shell.compose(
            state.reported_geometry.cols(),
            state.reported_geometry.rows(),
        )
    });
    if let Some(frame) = frame {
        state.present_frame(frame);
    }
    // A Local selection made while reconnecting is newer than this transaction's successor.
    if let Some(intent) = successor.filter(|_| state.deferred_local_activation.is_none()) {
        return Ok(Some(ClientLoopEvent::ActivateEndpoint {
            endpoint_id: intent.endpoint_id,
            target: intent.target,
            force: true,
        }));
    }
    Ok(None)
}

/// A handoff asks its endpoint for a surface sized by the shell layout of the projection that
/// was current when it started (the source's), and the committed projection can lay out
/// differently: with `hide_tab_bar_when_single_tab`, switching between a single-tab and a
/// multi-tab workspace moves the pane area by the tab bar's row. Nothing else resizes after the
/// commit (snapshot installs only compare their own before/after), so the committed endpoint
/// would keep panes one row off. Once the handoff has committed, compare the size it asked for
/// with the committed layout and resize the committed endpoint when they disagree.
fn correct_committed_surface_size(
    state: &ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    requested: Option<shepr_protocol::ClientSurfaceSize>,
) {
    if let Some(resize) = requested.and_then(|requested| committed_resize(state, requested)) {
        // A failed send surfaces through the registry's failure list.
        endpoints.send(&resize);
    }
}

fn committed_resize(
    state: &ClientState,
    requested: shepr_protocol::ClientSurfaceSize,
) -> Option<ClientMessage> {
    let shell = state.mode.shell()?;
    (shell.surface_size(
        state.reported_geometry.cols(),
        state.reported_geometry.rows(),
    ) != requested)
        .then(|| {
            client_shell_resize_message(
                shell,
                state.reported_geometry.cols(),
                state.reported_geometry.rows(),
                state.reported_geometry.cell_width(),
                state.reported_geometry.cell_height(),
                state.reported_geometry.exact,
            )
        })
}

pub(super) fn present_handoff_unavailable(state: &mut ClientState, message: String) {
    // An unavailable committed endpoint has no presentation lease. Keep all pane input and late
    // source output blocked, while allowing this client-owned chrome frame through the freeze.
    state.freeze_presentation();
    let frame = state.mode.shell_mut().and_then(|shell| {
        shell.receive_endpoint_unavailable(message);
        shell.compose(
            state.reported_geometry.cols(),
            state.reported_geometry.rows(),
        )
    });
    if let Some(frame) = frame {
        state.present_frozen_chrome(frame);
    }
}

/// The rollback reason when an endpoint a machine switch involves disconnects mid-switch.
/// `notice` is the same predicate the active-endpoint path shows after the label ("connection
/// was lost; reconnecting", "was removed or re-pointed"), so both read as one
/// sentence about the named machine.
fn handoff_interrupted_notice(label: &str, notice: &str) -> String {
    format!("machine switch interrupted: {label} {notice}")
}

pub(super) fn rollback_endpoint_activation(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    pending: &mut Option<endpoint::PendingEndpointActivation>,
    error: &str,
    source_release_rejected: bool,
) {
    let Some(activation) = pending.as_mut() else {
        return;
    };
    match activation.rollback(endpoints, error, source_release_rejected) {
        endpoint::ActivationRollback::Pending => state.freeze_presentation(),
        endpoint::ActivationRollback::Unavailable(message) => {
            *pending = None;
            // No endpoint has been proven safe to present. Keep pane input frozen, but render
            // the client-owned unavailable chrome rather than silently swallowing the error.
            present_handoff_unavailable(state, message);
        }
    }
}

pub(super) fn handle_endpoint_disconnect(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    endpoint_commands: &mut endpoint::commands::EndpointCommands,
    supervisors: &mut endpoint::EndpointSupervisors,
    pending_activation: &mut Option<endpoint::PendingEndpointActivation>,
    endpoint_id: &endpoint::ClientEndpointId,
    generation: u64,
    now: std::time::Instant,
    notice: &str,
) -> bool {
    supervisors.disconnected(endpoint_id, generation, now);
    if let Some(pending) = pending_activation
        .as_mut()
        .filter(|pending| pending.involves_endpoint(endpoint_id))
    {
        let label = state
            .mode
            .shell()
            .map_or("Endpoint", |shell| shell.endpoint_label(endpoint_id));
        let outcome = pending.endpoint_disconnected(
            endpoints,
            endpoint_id,
            handoff_interrupted_notice(label, notice),
        );
        match outcome {
            endpoint::ActivationRollback::Pending => {}
            endpoint::ActivationRollback::Unavailable(error) => {
                *pending_activation = None;
                present_handoff_unavailable(state, error);
            }
        }
    }
    let endpoint_was_active = endpoints.active_id() == endpoint_id;
    let cancelled = endpoint_commands.disconnect(endpoint_id);
    let unavailable = state.mode.shell_mut().and_then(|shell| {
        for request_id in cancelled {
            shell.cancel_endpoint_request(&request_id);
        }
        shell.mark_endpoint_disconnected(endpoint_id);
        endpoint_was_active.then(|| format!("{} {notice}", shell.endpoint_label(endpoint_id)))
    });
    if let Some(message) = unavailable {
        present_handoff_unavailable(state, message);
    } else if let Some(frame) = state.mode.shell_mut().and_then(|shell| {
        shell.compose(
            state.reported_geometry.cols(),
            state.reported_geometry.rows(),
        )
    }) {
        // A non-active machine going offline only changes its machine-list status.
        state.present_chrome(frame, pending_activation.is_some());
    }
    endpoint_was_active
}

/// A freeze with no handoff in flight is one `present_handoff_unavailable` left: no endpoint
/// has proved it owns the presentation, so pane input and output stay blocked. Usually the
/// owner's connection is gone, and its reconnection (a connection without a surface) or the
/// user's next pick starts the handoff that ends the freeze. When the selected endpoint is the
/// active one and its connection is still marked surface-active, neither happens: automatic
/// activation only targets connections without a surface, and picking the machine that is
/// already active is a no-op. Pane input would then stay frozen until the user picked some
/// other machine.
///
/// This schedules one forced handoff to that endpoint, which re-proves ownership with a
/// fresh surface round trip and commits (unfreezing) or reports why not. It fires once per
/// connection generation and frozen episode, so a handoff that fails again cannot loop.
pub(super) fn stale_freeze_recovery(
    state: &ClientState,
    endpoints: &endpoint::EndpointRegistry,
    selected: &endpoint::ClientEndpointId,
    handoff_busy: bool,
    attempted: &mut Option<(endpoint::ClientEndpointId, u64)>,
) -> Option<ClientLoopEvent> {
    if !state.presentation_frozen {
        *attempted = None;
        return None;
    }
    if handoff_busy || endpoints.active_id() != selected {
        return None;
    }
    let shell = state.mode.shell()?;
    let generation = endpoints
        .connection(selected)
        .filter(|connection| connection.surface_active)?
        .generation
        .get();
    // Without metadata for this connection the handoff could not even be prepared; the
    // snapshot that brings it also runs the ordinary activation check.
    shell.endpoint_snapshot_identity(selected, generation)?;
    let key = (selected.clone(), generation);
    if attempted.as_ref() == Some(&key) {
        return None;
    }
    *attempted = Some(key);
    Some(ClientLoopEvent::ActivateEndpoint {
        endpoint_id: selected.clone(),
        target: None,
        force: true,
    })
}

/// Makes an open client follow a newer copy of the saved-machine catalog: machines that
/// were removed or pointed at another target or session are disconnected and stop being
/// supervised; added or re-pointed ones start connecting; labels update.
/// Config is still read once at launch; the catalog is state that `shepr machine` edits.
///
/// Returns whether the endpoint that owned (or last owned) the presentation was retired.
/// The caller then clears its host effects and hands the presentation to Local, as if the
/// user had picked Local.
pub(super) fn follow_endpoint_catalog(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    endpoint_commands: &mut endpoint::commands::EndpointCommands,
    supervisors: &mut endpoint::EndpointSupervisors,
    pending_activation: &mut Option<endpoint::PendingEndpointActivation>,
    catalog: &mut endpoint::EndpointCatalog,
    local_socket_path: &std::path::Path,
    profiles: Vec<endpoint::SavedSshEndpoint>,
    now: std::time::Instant,
) -> bool {
    if profiles == catalog.ssh {
        return false;
    }
    let changes = endpoint::EndpointCatalogChanges::between(&catalog.ssh, &profiles);
    let mut active_retired = false;
    for profile_id in &changes.retired {
        let endpoint_id = endpoint::ClientEndpointId::Ssh(profile_id.clone());
        supervisors.retire(&endpoint_id);
        let generation = endpoints
            .connection(&endpoint_id)
            .map_or(0, |connection| connection.generation.get());
        active_retired |= handle_endpoint_disconnect(
            state,
            endpoints,
            endpoint_commands,
            supervisors,
            pending_activation,
            &endpoint_id,
            generation,
            now,
            "was removed or re-pointed",
        );
        endpoints.disconnect(&endpoint_id);
    }
    if let Some(shell) = state.mode.shell_mut() {
        // Retired machines are first dropped from the shell, which discards what it kept
        // from them (status, snapshot, agents). A re-pointed machine is then shown afresh
        // by the final catalog instead of with the old target's workspaces.
        if !changes.retired.is_empty() {
            let interim: Vec<_> = profiles
                .iter()
                .filter(|profile| !changes.retired.contains(&profile.id))
                .cloned()
                .collect();
            shell.set_endpoint_catalog(&interim);
        }
        shell.set_endpoint_catalog(&profiles);
    }
    for profile in &changes.started {
        supervisors.start_ssh(profile, now);
    }
    catalog.replace_profiles(profiles);
    // A client that launched with no saved machine had no Local supervisor: losing Local
    // ended it. Once a machine exists, Local is supervised like the rest, so its loss is
    // recovered instead of ending the client and taking the machine with it.
    if endpoint::LocalFailurePolicy::for_catalog(catalog).reconnects_local()
        && !supervisors.supervises(&endpoint::ClientEndpointId::Local)
    {
        supervisors.add_local(
            local_socket_path.to_path_buf(),
            endpoints
                .connection(&endpoint::ClientEndpointId::Local)
                .map(|connection| connection.generation.get()),
            now,
        );
    }
    if let Some(frame) = state.mode.shell_mut().and_then(|shell| {
        shell.compose(
            state.reported_geometry.cols(),
            state.reported_geometry.rows(),
        )
    }) {
        state.present_chrome(frame, pending_activation.is_some());
    }
    active_retired
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
        && !state.presentation_frozen
        && endpoints.active_id() == endpoint_id
        && connection.surface_active;
    let (composed, resize) = if let Some(shell) = state.mode.shell_mut() {
        let waits_for_selected_surface = projection_pending
            || (endpoints.active_id() == endpoint_id
                && !project_snapshot
                && shell.has_presented_surface());
        let previous_size = shell.surface_size(
            state.reported_geometry.cols(),
            state.reported_geometry.rows(),
        );
        if !waits_for_selected_surface {
            shell.set_endpoint_status(endpoint_id, endpoint::ClientEndpointStatus::Online);
        }
        if project_snapshot {
            shell.set_endpoint_snapshot_for_generation(endpoint_id, generation, snapshot);
        } else {
            shell.cache_endpoint_snapshot_for_generation(endpoint_id, generation, snapshot);
        }
        let next_size = shell.surface_size(
            state.reported_geometry.cols(),
            state.reported_geometry.rows(),
        );
        (
            shell.compose(
                state.reported_geometry.cols(),
                state.reported_geometry.rows(),
            ),
            (previous_size != next_size).then(|| {
                client_shell_resize_message(
                    shell,
                    state.reported_geometry.cols(),
                    state.reported_geometry.rows(),
                    state.reported_geometry.cell_width(),
                    state.reported_geometry.cell_height(),
                    state.reported_geometry.exact,
                )
            }),
        )
    } else {
        (None, None)
    };
    if let Some(resize) = resize {
        endpoints.send_to(endpoint_id, &resize);
    }
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
            state.present_frozen_chrome(frame);
        }
    }
    Ok(())
}

pub(super) fn finish_client_shell_input(
    state: &mut ClientState,
    outcome: shell::ClientShellInput,
    frame: Option<super::frame_output::ComposedFrame>,
    endpoints: &mut endpoint::EndpointRegistry,
    pending_activation: &mut Option<endpoint::PendingEndpointActivation>,
    endpoint_commands: &mut endpoint::commands::EndpointCommands,
    scheduled_activation: &mut Option<ClientLoopEvent>,
) -> Result<bool, ClientError> {
    if outcome.detach {
        // A failed send is recorded against the endpoint, and the registry's Drop sends
        // Detach again on the way out.
        endpoints.send(&ClientMessage::Detach);
        return Ok(true);
    }
    if outcome.resize
        && let Some(shell) = state.mode.shell()
    {
        let resize = client_shell_resize_message(
            shell,
            state.reported_geometry.cols(),
            state.reported_geometry.rows(),
            state.reported_geometry.cell_width(),
            state.reported_geometry.cell_height(),
            state.reported_geometry.exact,
        );
        if let Some(activation) = pending_activation.as_mut() {
            if let Err(error) = activation.update_resize(&resize, endpoints) {
                rollback_endpoint_activation(state, endpoints, pending_activation, &error, false);
            }
        } else {
            // A failed send is recorded against the active endpoint; the client timer
            // applies the reconnect or local failure policy to it.
            endpoints.send(&resize);
        }
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
        &mut state.output_writer,
        state.settings.prefers_osc52_clipboard(),
        state.mode.shell_mut(),
        scheduled_activation,
    );
    let frame = if dispatch_repaint {
        state.mode.shell_mut().and_then(|shell| {
            shell.compose(
                state.reported_geometry.cols(),
                state.reported_geometry.rows(),
            )
        })
    } else {
        frame
    };
    let active_endpoint_online = state
        .mode
        .shell()
        .is_none_or(|shell| shell.endpoint_is_online(endpoints.active_id()))
        && endpoints.active_surface_available();
    for request in outcome.requests {
        if let ClientMessage::ClientShellHostTheme { update } = &request {
            state.record_host_theme_update(update);
            if let Some(activation) = pending_activation.as_mut() {
                if let Err(error) = activation.update_host_theme(update.clone(), endpoints) {
                    rollback_endpoint_activation(
                        state,
                        endpoints,
                        pending_activation,
                        &error,
                        false,
                    );
                }
                continue;
            }
        }
        // Host focus belongs to a pending target even when the source has gone offline or has
        // already had its surface revoked. Route it before the ordinary source-online gate.
        if let ClientMessage::ClientShellFocus { focused } = request {
            if let Some(activation) = pending_activation.as_mut() {
                if let Err(error) = activation.update_host_focus(focused, endpoints) {
                    rollback_endpoint_activation(
                        state,
                        endpoints,
                        pending_activation,
                        &error,
                        false,
                    );
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
        if pending_activation.is_some() {
            // Pane input and non-focus host effects do not cross the frozen handoff boundary.
            continue;
        }
        write_to_server(endpoints, &request).map_err(ClientError::ConnectionLost)?;
    }
    if let Some(frame) = frame {
        // With no handoff in flight, input frames pass a freeze left by
        // `present_handoff_unavailable` so mode changes, overlays and the machine list stay
        // responsive while no endpoint owns presentation. That is sound because nothing moves
        // the pane projection while frozen (see `install_client_shell_snapshot` and the pane
        // surface arms of the client loop): the pane cells in this frame are the frozen ones.
        state.present_chrome(frame, pending_activation.is_some());
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_fixtures::*;

    #[test]
    fn committed_handoff_resizes_only_when_its_requested_size_is_stale() {
        let state = ClientState::test_new();
        let committed = state.mode.shell().expect("test shell").surface_size(
            state.reported_geometry.cols(),
            state.reported_geometry.rows(),
        );
        assert!(committed_resize(&state, committed).is_none());

        // A surface requested under the source's layout, one row off (the tab bar hides for a
        // single-tab workspace), is corrected to the committed layout.
        let stale = shepr_protocol::ClientSurfaceSize {
            cols: committed.cols,
            rows: committed.rows.saturating_add(1),
        };
        match committed_resize(&state, stale) {
            Some(ClientMessage::ClientShellResize { geometry }) => {
                assert_eq!(geometry.surface_size(), committed);
            }
            other => panic!("expected a corrective resize, got {other:?}"),
        }
    }

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
            boot_id: boot_id.into(),
            revision: shepr_protocol::ProjectionRevision::new(1),
            resolved_config: shepr_test_fixtures::encode_to_vec(
                &shepr_config::ValidatedConfig::test_default(),
            )
            .expect("test config encodes"),
            focused_workspace_id: None,
            focused_tab_id: None,
            focused_pane_id: None,
            tab_bar_right: Vec::new(),
            tab_bar_right_separator: String::new(),
            workspaces: Vec::new(),
            tabs: Vec::new(),
            panes: Vec::new(),
            agents: Vec::new(),
        })
    }

    fn is_forced_activation(
        event: Option<ClientLoopEvent>,
        expected: &endpoint::ClientEndpointId,
    ) -> bool {
        matches!(
            event,
            Some(ClientLoopEvent::ActivateEndpoint {
                endpoint_id,
                target: None,
                force: true,
            }) if &endpoint_id == expected
        )
    }

    #[test]
    fn chrome_frames_pass_an_unavailable_freeze_but_not_a_handoff() {
        let mut state = ClientState::test_new();
        let compose = |state: &mut ClientState| {
            let (cols, rows) = (
                state.reported_geometry.cols(),
                state.reported_geometry.rows(),
            );
            state
                .mode
                .shell_mut()
                .expect("test shell")
                .compose(cols, rows)
                .expect("test shell composes")
        };
        state.freeze_presentation();
        state.request_repaint();

        // During a handoff the source frame stays authoritative.
        let frame = compose(&mut state);
        state.present_chrome(frame, true);
        assert!(
            state.repaint_pending,
            "a handoff freeze must hold the frame back"
        );

        // With no handoff in flight (`present_handoff_unavailable`), machine statuses show.
        let frame = compose(&mut state);
        state.present_chrome(frame, false);
        assert!(!state.repaint_pending, "the chrome frame was presented");
        assert!(
            state.presentation_frozen,
            "pane input and output stay frozen"
        );
    }

    #[test]
    fn a_freeze_the_surface_owner_survived_is_recovered_once_per_episode() {
        let local = endpoint::ClientEndpointId::Local;
        let mut state = ClientState::test_new();
        state
            .mode
            .shell_mut()
            .expect("test shell")
            .set_endpoint_snapshot_for_generation(&local, 1, snapshot("local-boot"));
        let mut endpoints = endpoint::EndpointRegistry::new(NullTransport, 1);
        let mut attempted = None;

        // Nothing to recover while presentation is live.
        assert!(stale_freeze_recovery(&state, &endpoints, &local, false, &mut attempted).is_none());

        // `present_handoff_unavailable` froze presentation although Local still holds the
        // surface. A handoff in flight owns the freeze and is left alone.
        state.freeze_presentation();
        assert!(stale_freeze_recovery(&state, &endpoints, &local, true, &mut attempted).is_none());
        let other = endpoint::ClientEndpointId::Ssh(
            endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef")
                .expect("test precondition"),
        );
        assert!(
            stale_freeze_recovery(&state, &endpoints, &other, false, &mut attempted).is_none(),
            "a different selected machine is the ordinary activation path's job"
        );

        assert!(is_forced_activation(
            stale_freeze_recovery(&state, &endpoints, &local, false, &mut attempted),
            &local
        ));
        assert!(
            stale_freeze_recovery(&state, &endpoints, &local, false, &mut attempted).is_none(),
            "a recovery that fails again must not loop"
        );

        // A later episode may recover again.
        state.unfreeze_presentation();
        assert!(stale_freeze_recovery(&state, &endpoints, &local, false, &mut attempted).is_none());
        state.freeze_presentation();
        assert!(is_forced_activation(
            stale_freeze_recovery(&state, &endpoints, &local, false, &mut attempted),
            &local
        ));

        // A connection without a surface is reactivated by the ordinary snapshot path.
        state.unfreeze_presentation();
        stale_freeze_recovery(&state, &endpoints, &local, false, &mut attempted);
        state.freeze_presentation();
        endpoints.set_surface_active(&local, false);
        assert!(stale_freeze_recovery(&state, &endpoints, &local, false, &mut attempted).is_none());
    }

    #[test]
    fn an_open_client_follows_saved_machine_changes() {
        let now = std::time::Instant::now();
        let scratch = shepr_test_support::ScratchDir::new("endpoint-catalog-follow");
        let local_socket_path = scratch.join("shepr-client.sock");
        let mut catalog = endpoint::EndpointCatalog::default();
        let build = catalog
            .add_ssh("Build", "build", "agents")
            .expect("test precondition");
        let build_id = endpoint::ClientEndpointId::Ssh(build.clone());
        let local = endpoint::ClientEndpointId::Local;

        let mut state = ClientState::test_new();
        let shell = state.mode.shell_mut().expect("test shell");
        shell.set_endpoint_catalog(&catalog.ssh);
        let mut endpoints = endpoint::EndpointRegistry::new(NullTransport, 1);
        endpoints.insert(build_id.clone(), NullTransport, 7, true);
        assert!(endpoints.set_active(&build_id));
        let mut commands = endpoint::commands::EndpointCommands::default();
        let mut pending = None;
        let mut supervisors = endpoint::EndpointSupervisors::with_ssh_settings(
            &shepr_config::AppPaths::test_default(),
            &catalog.ssh,
            shepr_remote::SavedSshSettings {
                manage_ssh_config: false,
            },
            now,
        )
        .expect("test saved SSH setup is retryable");

        // The active machine is re-pointed at another target and another machine is added.
        let mut added = endpoint::EndpointCatalog::default();
        let docs = added
            .add_ssh("Docs", "docs", "default")
            .expect("test precondition");
        let docs_id = endpoint::ClientEndpointId::Ssh(docs);
        let mut profiles = catalog.ssh.clone();
        profiles[0].target =
            shepr_remote::SshTarget::parse("build-moved").expect("test precondition");
        profiles.extend(added.ssh);

        assert!(
            follow_endpoint_catalog(
                &mut state,
                &mut endpoints,
                &mut commands,
                &mut supervisors,
                &mut pending,
                &mut catalog,
                &local_socket_path,
                profiles.clone(),
                now,
            ),
            "retiring the active machine hands presentation back to Local"
        );
        assert!(endpoints.connection(&build_id).is_none());
        assert!(
            supervisors.supervises(&build_id),
            "a re-pointed machine is retired and started again"
        );
        assert!(supervisors.supervises(&docs_id));
        assert!(
            supervisors.supervises(&local),
            "Local is supervised once the client has a saved machine"
        );
        assert_eq!(catalog.ssh, profiles);
        assert!(
            state.presentation_frozen,
            "nothing owns presentation until Local commits"
        );
        let shell = state.mode.shell().expect("test shell");
        assert_eq!(
            shell.endpoint_status(&build_id),
            Some(endpoint::ClientEndpointStatus::Connecting),
            "the re-pointed machine is shown afresh, not with the old target's state"
        );
        assert_eq!(
            shell.endpoint_status(&docs_id),
            Some(endpoint::ClientEndpointStatus::Connecting)
        );

        // Seeing the same catalog again changes nothing.
        assert!(!follow_endpoint_catalog(
            &mut state,
            &mut endpoints,
            &mut commands,
            &mut supervisors,
            &mut pending,
            &mut catalog,
            &local_socket_path,
            profiles,
            now,
        ));
        assert!(supervisors.supervises(&docs_id));

        // `shepr machine remove` for every machine; none of them is active.
        assert!(endpoints.set_active(&local));
        assert!(!follow_endpoint_catalog(
            &mut state,
            &mut endpoints,
            &mut commands,
            &mut supervisors,
            &mut pending,
            &mut catalog,
            &local_socket_path,
            Vec::new(),
            now,
        ));
        assert!(!supervisors.supervises(&docs_id));
        assert!(!supervisors.supervises(&build_id));
        assert!(!catalog.has_ssh());
        assert!(
            state
                .mode
                .shell()
                .expect("test shell")
                .endpoint_status(&docs_id)
                .is_none()
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
