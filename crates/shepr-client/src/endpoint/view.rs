use super::{
    ClientEndpointId, ClientEndpointStatus, Committed, EndpointChoice, EndpointRegistry, ViewLease,
};
use crate::shell::ClientShellState;
use shepr_protocol::{
    BootId, ClientHostThemeUpdate, ClientMessage, ClientSurfaceSize, RequestId, TerminalGeometry,
    command::{ClientShellSurfaceSetParams, EndpointCommand},
};
use std::time::Instant;

/// What a newly viewed connection is told about the host before its on request.
pub struct HostBaseline<'a> {
    /// The one surface geometry every endpoint renders.
    pub geometry: TerminalGeometry,
    /// The host focus baseline. Not sent by `turn_on`: focus follows the commit.
    pub host_focused: bool,
    /// Every recorded host theme update, replayed in order.
    pub theme: &'a [ClientHostThemeUpdate],
}

/// What `start_move` did.
#[derive(Debug, PartialEq, Eq)]
pub enum StartOutcome {
    /// No move may start (Showing, Preparing, or Failed without a new generation).
    Idle,
    /// The move stays Waiting: `to` has no connection and is Local or nothing is shown, or
    /// `to` has no metadata for its current generation.
    Waiting,
    /// A machine without a connection while something is shown: the choice is back on
    /// `from`; the caller shows "{label} is not ready".
    Abandoned(ClientEndpointId),
    /// `begin_preparing` ran, then `turn_on`. A failed send is already recorded as a
    /// connection failure and is handled as a lost target next turn.
    Started,
}

/// The viewing request: `active: true` turns a connection on, `false` turns it off.
pub(crate) fn surface_interest_request(
    boot_id: &BootId,
    request_id: RequestId,
    active: bool,
) -> ClientMessage {
    ClientMessage::ClientShellEndpointRequest {
        boot_id: boot_id.clone(),
        request_id,
        command: EndpointCommand::ClientShellSurfaceSet(ClientShellSurfaceSetParams { active }),
    }
}

/// Reconcile step 2. Starts a `Waiting` move (or a `Failed` one on another connection
/// generation) once `to` has a connection with metadata for its generation. The lease comes
/// from that metadata; `begin_preparing` runs before `turn_on`, so every send happens with the
/// `Preparing` installed.
pub fn start_move(
    choice: &mut EndpointChoice,
    endpoints: &mut EndpointRegistry,
    shell: &ClientShellState,
    baseline: &HostBaseline<'_>,
    serial: &mut u64,
    now: Instant,
) -> StartOutcome {
    let Some(pending) = choice.pending_start() else {
        return StartOutcome::Idle;
    };
    let Some(connection) = endpoints.connection(pending.to) else {
        if !pending.to.is_local()
            && pending.from.is_some()
            && let Some(to) = choice.abandon()
        {
            return StartOutcome::Abandoned(to);
        }
        return StartOutcome::Waiting;
    };
    let generation = connection.generation.get();
    if pending.failed_generation == Some(generation) {
        return StartOutcome::Idle;
    }
    let Some((boot, minimum_revision)) = shell.endpoint_snapshot_identity(pending.to, generation)
    else {
        return StartOutcome::Waiting;
    };
    let lease = ViewLease {
        endpoint_id: pending.to.clone(),
        generation,
        boot_id: boot.clone(),
        minimum_revision,
    };
    let request: RequestId = format!("client-shell-view:{serial}:on").into();
    *serial = serial.saturating_add(1);
    choice.begin_preparing(lease.clone(), request.clone(), baseline.geometry, now);
    turn_on(endpoints, &lease, &request, baseline);
    StartOutcome::Started
}

/// Records the connection viewed, then sends the resize, every recorded theme update and the
/// on request. No focus message: focus follows the commit. Always sends a fresh request, also
/// to a connection already viewed: that is what makes turning on idempotent and gives the new
/// epoch its own floor.
pub(crate) fn turn_on(
    endpoints: &mut EndpointRegistry,
    lease: &ViewLease,
    request: &RequestId,
    baseline: &HostBaseline<'_>,
) {
    endpoints.set_viewed(&lease.endpoint_id, true);
    endpoints.send_to(
        &lease.endpoint_id,
        &ClientMessage::ClientShellResize {
            geometry: baseline.geometry,
        },
    );
    for update in baseline.theme {
        endpoints.send_to(
            &lease.endpoint_id,
            &ClientMessage::ClientShellHostTheme {
                update: update.clone(),
            },
        );
    }
    endpoints.send_to(
        &lease.endpoint_id,
        &surface_interest_request(&lease.boot_id, request.clone(), true),
    );
}

/// Reconcile step 3. Sends the target's next navigation request, if its focus lane has one.
pub fn send_focus(choice: &mut EndpointChoice, endpoints: &mut EndpointRegistry) {
    if let Some(preparing) = choice.preparing_mut()
        && let Some(request) = preparing.focus_request()
    {
        endpoints.send_to(&preparing.lease().endpoint_id, &request);
    }
}

/// Reconcile step 4. `Ok(None)`: nothing is ready. `Err(reason)`: a commit precondition
/// failed and nothing was changed except, at most, a correct Online status. On success the
/// shell projects the target, the choice shows it, and the target is sent the host focus
/// baseline and `ReplayHostEffects`; a failed send there does not undo the switch, it is a
/// connection failure handled next turn.
pub fn commit_move(
    choice: &mut EndpointChoice,
    endpoints: &mut EndpointRegistry,
    shell: &mut ClientShellState,
    host_focused: bool,
    size: ClientSurfaceSize,
) -> Result<Option<Committed>, String> {
    let Some(preparing) = choice.preparing() else {
        return Ok(None);
    };
    let Some(surface) = preparing.ready(size) else {
        return Ok(None);
    };
    let lease = preparing.lease();
    // A failed send removes the connection at once and queues its failure for the next
    // turn. That loss must decide the move (an interrupted switch), not a commit to a
    // connection that is gone or a failure reported against it.
    if !endpoints.accepts(&lease.endpoint_id, lease.generation)
        || !endpoints.viewed(&lease.endpoint_id)
    {
        return Ok(None);
    }
    if !shell.endpoint_snapshot_matches(
        &lease.endpoint_id,
        lease.generation,
        &lease.boot_id,
        surface.projection_revision.get(),
    ) {
        return Err("endpoint move lost its coherent snapshot/surface pair".into());
    }
    let target = lease.endpoint_id.clone();
    shell.set_endpoint_status(&target, ClientEndpointStatus::Online);
    if !shell.endpoint_projection_available(&target) || !shell.activate_endpoint_projection(&target)
    {
        return Err("endpoint projection is unavailable".into());
    }
    shell.receive_pane_surface_from(surface.clone(), lease.generation);
    let committed = choice.commit();
    endpoints.send_to(
        &target,
        &ClientMessage::ClientShellFocus {
            focused: host_focused,
        },
    );
    endpoints.send_to(&target, &ClientMessage::ReplayHostEffects);
    Ok(committed)
}

/// Reconcile step 5. Turns off every viewed connection that is neither shown nor the target
/// being prepared; see `EndpointRegistry::release_unwanted_views`.
pub fn release_unwanted(
    choice: &EndpointChoice,
    endpoints: &mut EndpointRegistry,
    shell: &ClientShellState,
    serial: &mut u64,
) -> usize {
    endpoints.release_unwanted_views(
        |id| choice.wants_view(id),
        |id| shell.endpoint_boot_id(id),
        serial,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::endpoint_choice::{Fixture, RecordingTransport, boot, remote, snapshot};
    fn start(f: &mut Fixture) -> StartOutcome {
        let baseline = HostBaseline {
            geometry: crate::shell_runtime::view_geometry(&f.client.state),
            host_focused: true,
            theme: &f.client.state.host_theme_updates,
        };
        start_move(
            &mut f.client.state.choice,
            &mut f.client.write_stream,
            &f.client.state.shell,
            &baseline,
            &mut f.client.next_view_serial,
            f.now,
        )
    }
    #[test]
    fn turn_on_sends_geometry_then_theme_then_the_request_and_no_focus() {
        let mut f = Fixture::new();
        f.client
            .state
            .host_theme_updates
            .push(ClientHostThemeUpdate::Appearance(
                shepr_protocol::ClientHostAppearance::Dark,
            ));
        f.pick(remote());
        assert_eq!(start(&mut f), StartOutcome::Started);
        assert!(matches!(
            f.target.take().as_slice(),
            [
                ClientMessage::ClientShellResize { .. },
                ClientMessage::ClientShellHostTheme { .. },
                ClientMessage::ClientShellEndpointRequest {
                    command: EndpointCommand::ClientShellSurfaceSet(ClientShellSurfaceSetParams {
                        active: true
                    }),
                    ..
                }
            ]
        ));
    }
    #[test]
    fn turn_on_to_an_already_viewed_connection_still_sends_a_fresh_request() {
        let mut f = Fixture::new();
        f.pick(remote());
        f.client.write_stream.set_viewed(&remote(), true);
        assert_eq!(start(&mut f), StartOutcome::Started);
        let first = f.on_request();
        f.client.state.choice.fail_move();
        f.pick(remote());
        assert_eq!(start(&mut f), StartOutcome::Started);
        assert_ne!(first, f.on_request());
    }
    #[test]
    fn start_move_installs_preparing_before_it_sends() {
        let mut f = Fixture::new();
        f.pick(remote());
        f.target.fail_next();
        assert_eq!(start(&mut f), StartOutcome::Started);
        assert!(f.client.state.choice.preparing().is_some());
        assert!(f.client.write_stream.connection(&remote()).is_none());
        assert_eq!(f.client.write_stream.take_failures().len(), 1);
    }
    #[test]
    fn start_move_abandons_a_machine_without_a_connection_while_something_is_shown() {
        let mut f = Fixture::new();
        f.client.write_stream.disconnect(&remote());
        f.pick(remote());
        assert_eq!(start(&mut f), StartOutcome::Abandoned(remote()));
        assert_eq!(
            f.client.state.choice.shown(),
            Some(&ClientEndpointId::Local)
        );
    }
    #[test]
    fn start_move_waits_for_metadata_of_the_current_generation() {
        let mut f = Fixture::new();
        f.client
            .write_stream
            .insert(remote(), f.target.clone(), 8, false, f.now);
        f.pick(remote());
        assert_eq!(start(&mut f), StartOutcome::Waiting);
        assert!(f.target.take().is_empty());
    }
    #[test]
    fn start_move_restarts_a_failed_move_only_on_another_generation() {
        let mut f = Fixture::new();
        f.client.state.choice = EndpointChoice::waiting_for(remote());
        assert_eq!(start(&mut f), StartOutcome::Started);
        f.client.state.choice.fail_move();
        f.target.take();
        assert_eq!(start(&mut f), StartOutcome::Idle);
        assert!(f.target.take().is_empty());
        f.client
            .write_stream
            .insert(remote(), RecordingTransport::default(), 8, false, f.now);
        assert_eq!(start(&mut f), StartOutcome::Waiting);
        f.client.state.shell.cache_endpoint_snapshot_for_generation(
            &remote(),
            8,
            snapshot(&remote(), 1),
        );
        assert_eq!(start(&mut f), StartOutcome::Started);
    }
    #[test]
    fn commit_move_changes_nothing_when_a_check_fails() {
        let mut f = Fixture::new();
        f.start();
        f.evidence();
        f.client.state.shell.cache_endpoint_snapshot_for_generation(
            &remote(),
            7,
            snapshot(&remote(), 3),
        );
        let size = f.size();
        let messages = f.target.take();
        assert!(!messages.is_empty());
        assert!(
            commit_move(
                &mut f.client.state.choice,
                &mut f.client.write_stream,
                &mut f.client.state.shell,
                true,
                size
            )
            .is_err()
        );
        assert_eq!(
            f.client.state.choice.shown(),
            Some(&ClientEndpointId::Local)
        );
        assert!(
            f.client
                .state
                .shell
                .endpoint_is_active(&ClientEndpointId::Local)
        );
        assert!(f.target.take().is_empty());
    }
    #[test]
    fn commit_move_sends_the_focus_baseline_then_replay() {
        let mut f = Fixture::new();
        f.start();
        f.evidence();
        f.target.take();
        let size = f.size();
        assert!(
            commit_move(
                &mut f.client.state.choice,
                &mut f.client.write_stream,
                &mut f.client.state.shell,
                false,
                size
            )
            .expect("commit")
            .is_some()
        );
        assert!(matches!(
            f.target.take().as_slice(),
            [
                ClientMessage::ClientShellFocus { focused: false },
                ClientMessage::ReplayHostEffects
            ]
        ));
    }
    #[test]
    fn a_failed_commit_send_still_completes_the_switch() {
        let mut f = Fixture::new();
        f.start();
        f.evidence();
        f.target.fail_next();
        let size = f.size();
        assert!(
            commit_move(
                &mut f.client.state.choice,
                &mut f.client.write_stream,
                &mut f.client.state.shell,
                true,
                size
            )
            .expect("commit")
            .is_some()
        );
        assert_eq!(f.client.state.choice.shown(), Some(&remote()));
        assert_eq!(f.client.write_stream.take_failures().len(), 1);
    }
    #[test]
    fn release_unwanted_sends_focus_loss_then_the_release() {
        let mut f = Fixture::new();
        f.start();
        f.pick(ClientEndpointId::Local);
        assert_eq!(
            release_unwanted(
                &f.client.state.choice,
                &mut f.client.write_stream,
                &f.client.state.shell,
                &mut f.client.next_view_serial
            ),
            1
        );
        let sent = f.target.take();
        assert!(
            matches!(&sent[sent.len()-2..], [ClientMessage::ClientShellFocus { focused: false }, ClientMessage::ClientShellEndpointRequest { boot_id, command: EndpointCommand::ClientShellSurfaceSet(ClientShellSurfaceSetParams { active: false }), .. }] if boot_id == &boot(&remote()))
        );
    }
}
