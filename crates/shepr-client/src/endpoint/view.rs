//! The I/O steps of an endpoint move. `EndpointHub::reconcile` sequences them: failures
//! first, then start, focus, commit and release, in the order their doc comments number.

use super::{ClientEndpointId, Committed, EndpointChoice, EndpointRegistry, ViewLease};
use crate::shell::ClientShellState;
use shepr_protocol::{
    BootId, ClientHostThemeUpdate, ClientMessage, RequestId, TerminalGeometry,
    command::{ClientShellSurfaceSetParams, EndpointCommand},
};
use std::time::Instant;

/// What a newly viewed connection is told about the host before its on request.
pub(crate) struct HostBaseline<'a> {
    /// The one surface geometry every endpoint renders.
    pub(crate) geometry: TerminalGeometry,
    /// Every recorded host theme update, replayed in order.
    pub(crate) theme: &'a [ClientHostThemeUpdate],
}

/// What `start_move` did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StartOutcome {
    /// No move may start (Showing, Preparing, or Failed without a new generation).
    Idle,
    /// The move stays Waiting: `to` has no connection and is Local or nothing is shown, or
    /// `to` has no metadata for its current generation.
    Waiting,
    /// A machine without a connection while something is shown: the choice is back on
    /// `from`; the caller shows the target's not-ready notice.
    Abandoned(ClientEndpointId),
    /// `begin_preparing` ran, then `turn_on`. A failed send is already recorded as a
    /// connection failure and is handled as a lost target next turn.
    Started,
}

enum TargetReadiness {
    Waiting,
    Abandon,
    FailedGeneration,
    Ready {
        generation: shepr_protocol::ConnectionGeneration,
        boot_id: BootId,
        minimum_revision: shepr_protocol::ProjectionRevision,
    },
}

/// The action notice and reconcile use one assessment, so a pick cannot promise to wait when
/// the next reconcile will abandon it, or report a wait when its current connection is ready.
fn target_readiness(
    target: &ClientEndpointId,
    has_shown_endpoint: bool,
    failed_generation: Option<shepr_protocol::ConnectionGeneration>,
    endpoints: &EndpointRegistry,
    shell: &ClientShellState,
) -> TargetReadiness {
    let Some(connection) = endpoints.connection(target) else {
        return if target
            .policy()
            .abandons_unconnected_move(has_shown_endpoint)
        {
            TargetReadiness::Abandon
        } else {
            TargetReadiness::Waiting
        };
    };
    let generation = connection.generation;
    if failed_generation == Some(generation) {
        return TargetReadiness::FailedGeneration;
    }
    let Some((boot_id, minimum_revision)) = shell.endpoint_snapshot_identity(target, generation)
    else {
        return TargetReadiness::Waiting;
    };
    TargetReadiness::Ready {
        generation,
        boot_id: boot_id.clone(),
        minimum_revision,
    }
}

/// Whether a shell pick needs its waiting notice. This uses the same target assessment as the
/// reconcile step that starts or abandons the move.
pub(crate) fn selection_wait_notice_needed(
    target: &ClientEndpointId,
    choice: &EndpointChoice,
    endpoints: &EndpointRegistry,
    shell: &ClientShellState,
) -> bool {
    matches!(
        target_readiness(target, choice.live().is_some(), None, endpoints, shell),
        TargetReadiness::Waiting
    )
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
pub(crate) fn start_move<'a>(
    endpoints: &mut EndpointRegistry,
    shell: &mut ClientShellState,
    baseline: impl FnOnce(&ClientShellState) -> HostBaseline<'a>,
    now: Instant,
) -> StartOutcome {
    let Some(pending) = shell.endpoints.choice.pending_start() else {
        return StartOutcome::Idle;
    };
    let (generation, boot_id, minimum_revision) = match target_readiness(
        pending.to,
        pending.from.is_some(),
        pending.failed_generation,
        endpoints,
        shell,
    ) {
        TargetReadiness::Waiting => return StartOutcome::Waiting,
        TargetReadiness::Abandon => {
            return shell
                .endpoints
                .choice
                .abandon()
                .map_or(StartOutcome::Waiting, StartOutcome::Abandoned);
        }
        TargetReadiness::FailedGeneration => return StartOutcome::Idle,
        TargetReadiness::Ready {
            generation,
            boot_id,
            minimum_revision,
        } => (generation, boot_id, minimum_revision),
    };
    let target = pending.to.clone();
    let baseline = baseline(shell);
    let lease = ViewLease {
        endpoint_id: target,
        generation,
        boot_id,
        minimum_revision,
    };
    let request = RequestId::allocate();
    shell
        .endpoints
        .choice
        .begin_preparing(lease.clone(), request.clone(), baseline.geometry, now);
    turn_on(endpoints, &lease, &request, &baseline);
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
pub(crate) fn send_focus(choice: &mut EndpointChoice, endpoints: &mut EndpointRegistry) {
    if let Some(preparing) = choice.preparing_mut()
        && let Some(request) = preparing.focus_request()
    {
        endpoints.send_to(&preparing.lease().endpoint_id, &request);
    }
}

/// Reconcile step 4. `Ok(None)`: nothing is ready. `Err(reason)`: a commit precondition
/// failed and nothing was changed except, at most, a correct Online status. On success the
/// choice shows the target, the shell projects it, and the target is sent the host focus
/// baseline and `ReplayHostEffects`; a failed send there does not undo the switch, it is a
/// connection failure handled next turn. This is the one place that commits the choice.
pub(crate) fn commit_move(
    endpoints: &mut EndpointRegistry,
    shell: &mut ClientShellState,
    host_focused: bool,
) -> Result<Option<Committed>, super::choice::MoveFailure> {
    let Some(preparing) = shell.endpoints.choice.preparing() else {
        return Ok(None);
    };
    let Some(surface) = preparing.ready() else {
        return Ok(None);
    };
    let lease = preparing.lease().clone();
    let surface = surface.clone();
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
        surface.projection_revision,
    ) {
        return Err(super::choice::MoveFailure::LostPair);
    }
    let target = lease.endpoint_id.clone();
    let Some(projection) = shell.endpoint_projection(&target) else {
        return Err(super::choice::MoveFailure::ProjectionUnavailable);
    };
    // Every check precedes this commit: past it the choice shows the target.
    let Some(committed) = shell.endpoints.choice.commit() else {
        return Ok(None);
    };
    shell.present_projection(&projection);
    shell.receive_pane_surface_from(surface, lease.generation);
    let committed = Some(committed);
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
pub(crate) fn release_unwanted(
    choice: &EndpointChoice,
    endpoints: &mut EndpointRegistry,
    shell: &ClientShellState,
) -> usize {
    endpoints.release_unwanted_views(|id| choice.wants_view(id), |id| shell.endpoint_boot_id(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoint::{EndpointFailureStatus, MoveFailure, PrepareProgress};
    use crate::shell::{ClientShellConfig, Location};
    use crate::tests::endpoints::{RecordingTransport, boot, remote, snapshot, surface};
    use crate::tests::test_generation;
    use shepr_core::geometry::{GridSize, HostCell, HostGeometry};
    use shepr_protocol::ClientSurfaceSize;
    use shepr_protocol::command::EndpointReply;
    use shepr_test_fixtures::ValidatedClientConfigFixture as _;

    /// The shell and the connections a move's steps act on, on a 100x30 host: Local shown,
    /// connected at generation 1 and viewed, and the machine `build` connected at generation
    /// 7 and not viewed, each with a snapshot at revision 1.
    struct Views {
        shell: ClientShellState,
        registry: EndpointRegistry,
        local: RecordingTransport,
        target: RecordingTransport,
        host: HostGeometry,
        theme: Vec<ClientHostThemeUpdate>,
        now: Instant,
    }

    impl Views {
        fn new() -> Self {
            let now = Instant::now();
            let config = shepr_config::ValidatedClientConfig::test_default();
            let host = HostGeometry::new(GridSize::clamped(100, 30), HostCell::Unknown);
            let mut shell =
                ClientShellState::new(ClientShellConfig::from_validated_config(&config));
            shell.set_machines(&[shepr_config::MachineConfig {
                label: shepr_config::MachineLabel::parse("build").expect("machine"),
                ssh: shepr_config::SshTarget::parse("host").expect("SSH"),
                palette: None,
            }]);
            shell.endpoint_connected(&ClientEndpointId::Local, test_generation(1));
            shell.set_endpoint_snapshot_for_generation(
                &ClientEndpointId::Local,
                test_generation(1),
                snapshot(&ClientEndpointId::Local, 1),
            );
            shell.endpoint_connected(&remote(), test_generation(7));
            shell.cache_endpoint_snapshot_for_generation(
                &remote(),
                test_generation(7),
                snapshot(&remote(), 1),
            );
            let size = shell.surface_size(host.cols(), host.rows());
            shell.receive_pane_surface_from(
                surface(&ClientEndpointId::Local, 1, size, "SOURCE"),
                test_generation(1),
            );
            let local = RecordingTransport::default();
            let target = RecordingTransport::default();
            let mut registry = EndpointRegistry::new_at(local.clone(), test_generation(1), now);
            registry.insert(remote(), target.clone(), test_generation(7), false, now);
            Self {
                shell,
                registry,
                local,
                target,
                host,
                theme: Vec::new(),
                now,
            }
        }

        fn size(&self) -> ClientSurfaceSize {
            self.shell.surface_size(self.host.cols(), self.host.rows())
        }

        fn pick(&mut self, id: ClientEndpointId) {
            self.shell.endpoints.choice.select(Location::machine(id));
        }

        /// Step 2 with the host baseline the loop derives.
        fn start(&mut self) -> StartOutcome {
            let host = self.host;
            let theme = &self.theme;
            start_move(
                &mut self.registry,
                &mut self.shell,
                |shell: &ClientShellState| HostBaseline {
                    geometry: crate::shell_runtime::view_geometry(
                        host,
                        shell.surface_size(host.cols(), host.rows()),
                    ),
                    theme,
                },
                self.now,
            )
        }

        /// Picks the machine and starts the move to it.
        fn start_remote(&mut self) {
            self.pick(remote());
            assert_eq!(self.start(), StartOutcome::Started);
        }

        /// The evidence `id` answers its last on request with, kept the way the hub admits
        /// it: the acknowledgement at projection revision `revision`, then the snapshot of
        /// that revision (also cached for the commit), then its surface.
        fn evidence(&mut self, id: &ClientEndpointId, revision: u64) {
            let request = on_request(if id.is_local() {
                &self.local
            } else {
                &self.target
            });
            self.evidence_for(id, &request, revision);
        }

        /// As `evidence`, answering the on request `request`.
        fn evidence_for(&mut self, id: &ClientEndpointId, request: &RequestId, revision: u64) {
            let generation = self.registry.connection(id).expect("connection").generation;
            let size = self.size();
            let snapshot = snapshot(id, revision);
            let preparing = self
                .shell
                .endpoints
                .choice
                .preparing_mut()
                .expect("preparing");
            assert_eq!(
                preparing.receive_response(
                    id,
                    generation,
                    &boot(id),
                    request,
                    Ok(EndpointReply::ClientShellSurfaceSet {
                        active: true,
                        projection_revision: shepr_test_fixtures::counter_at(revision),
                    }),
                ),
                PrepareProgress::Pending
            );
            assert_eq!(
                preparing.receive_snapshot(id, generation, &snapshot),
                PrepareProgress::Pending
            );
            assert_eq!(
                preparing.receive_surface(id, generation, surface(id, revision, size, "TARGET")),
                PrepareProgress::Pending
            );
            self.shell
                .cache_endpoint_snapshot_for_generation(id, generation, snapshot);
        }
    }

    /// The last view-on request `transport` carried.
    fn on_request(transport: &RecordingTransport) -> RequestId {
        transport
            .sent
            .lock()
            .expect("messages")
            .iter()
            .rev()
            .find_map(|m| match m {
                ClientMessage::ClientShellEndpointRequest {
                    request_id,
                    command: EndpointCommand::ClientShellSurfaceSet(p),
                    ..
                } if p.active => Some(request_id.clone()),
                _ => None,
            })
            .expect("on request")
    }

    #[test]
    fn turn_on_sends_geometry_then_theme_then_the_request_and_no_focus() {
        let mut f = Views::new();
        f.theme.push(ClientHostThemeUpdate::Appearance(
            shepr_protocol::ClientHostAppearance::Dark,
        ));
        f.pick(remote());
        assert_eq!(f.start(), StartOutcome::Started);
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
        let mut f = Views::new();
        f.pick(remote());
        f.registry.set_viewed(&remote(), true);
        assert_eq!(f.start(), StartOutcome::Started);
        let first = on_request(&f.target);
        f.shell.endpoints.choice.fail_move();
        f.pick(remote());
        assert_eq!(f.start(), StartOutcome::Started);
        assert_ne!(first, on_request(&f.target));
    }
    #[test]
    fn start_move_installs_preparing_before_it_sends() {
        let mut f = Views::new();
        f.pick(remote());
        f.target.fail_next();
        assert_eq!(f.start(), StartOutcome::Started);
        assert!(f.shell.endpoints.choice.preparing().is_some());
        assert!(f.registry.connection(&remote()).is_none());
        assert_eq!(f.registry.take_failures().len(), 1);
    }
    #[test]
    fn start_move_abandons_a_machine_without_a_connection_while_something_is_shown() {
        let mut f = Views::new();
        f.registry.disconnect(&remote());
        f.pick(remote());
        assert_eq!(f.start(), StartOutcome::Abandoned(remote()));
        assert_eq!(
            f.shell.endpoints.choice.live(),
            Some(&ClientEndpointId::Local)
        );
    }
    #[test]
    fn start_move_waits_for_metadata_of_the_current_generation() {
        let mut f = Views::new();
        f.registry
            .insert(remote(), f.target.clone(), test_generation(8), false, f.now);
        f.pick(remote());
        assert_eq!(f.start(), StartOutcome::Waiting);
        assert!(f.target.take().is_empty());
    }
    #[test]
    fn start_move_restarts_a_failed_move_only_on_another_generation() {
        let mut f = Views::new();
        // Nothing shown, waiting for the machine: a failed move then waits for a new
        // generation instead of returning to a shown endpoint.
        f.shell.endpoints.choice = EndpointChoice::waiting_for(remote());
        assert_eq!(f.start(), StartOutcome::Started);
        f.shell.endpoints.choice.fail_move();
        f.target.take();
        assert_eq!(f.start(), StartOutcome::Idle);
        assert!(f.target.take().is_empty());
        f.registry.insert(
            remote(),
            RecordingTransport::default(),
            test_generation(8),
            false,
            f.now,
        );
        assert_eq!(f.start(), StartOutcome::Waiting);
        f.shell.cache_endpoint_snapshot_for_generation(
            &remote(),
            test_generation(8),
            snapshot(&remote(), 1),
        );
        assert_eq!(f.start(), StartOutcome::Started);
    }
    #[test]
    fn commit_move_changes_nothing_when_a_check_fails() {
        let mut f = Views::new();
        f.start_remote();
        f.evidence(&remote(), 2);
        f.shell.cache_endpoint_snapshot_for_generation(
            &remote(),
            test_generation(7),
            snapshot(&remote(), 3),
        );
        let messages = f.target.take();
        assert!(!messages.is_empty());
        assert!(commit_move(&mut f.registry, &mut f.shell, true).is_err());
        assert_eq!(
            f.shell.endpoints.choice.live(),
            Some(&ClientEndpointId::Local)
        );
        assert!(f.shell.endpoint_is_active(&ClientEndpointId::Local));
        assert!(f.target.take().is_empty());
    }
    #[test]
    fn commit_move_sends_the_focus_baseline_then_replay() {
        let mut f = Views::new();
        f.start_remote();
        f.evidence(&remote(), 2);
        f.target.take();
        assert!(
            commit_move(&mut f.registry, &mut f.shell, false)
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
        let mut f = Views::new();
        f.start_remote();
        f.evidence(&remote(), 2);
        f.target.fail_next();
        assert!(
            commit_move(&mut f.registry, &mut f.shell, true)
                .expect("commit")
                .is_some()
        );
        assert_eq!(f.shell.endpoints.choice.live(), Some(&remote()));
        assert_eq!(f.registry.take_failures().len(), 1);
    }
    #[test]
    fn a_commit_whose_projection_is_unavailable_leaves_the_move_preparing() {
        let mut f = Views::new();
        f.start_remote();
        f.evidence(&remote(), 2);
        // The target fails after its evidence arrived: its projection is no longer usable.
        f.shell
            .set_endpoint_status(&remote(), EndpointFailureStatus::Reconnecting);
        assert!(matches!(
            commit_move(&mut f.registry, &mut f.shell, true),
            Err(MoveFailure::ProjectionUnavailable)
        ));
        assert!(f.shell.endpoints.choice.preparing().is_some());
        assert_eq!(
            f.shell.endpoints.choice.live(),
            Some(&ClientEndpointId::Local)
        );
    }
    #[test]
    fn release_unwanted_sends_focus_loss_then_the_release() {
        let mut f = Views::new();
        f.start_remote();
        f.pick(ClientEndpointId::Local);
        assert_eq!(
            release_unwanted(&f.shell.endpoints.choice, &mut f.registry, &f.shell),
            1
        );
        let sent = f.target.take();
        assert!(
            matches!(&sent[sent.len()-2..], [ClientMessage::ClientShellFocus { focused: false }, ClientMessage::ClientShellEndpointRequest { boot_id, command: EndpointCommand::ClientShellSurfaceSet(ClientShellSurfaceSetParams { active: false }), .. }] if boot_id == &boot(&remote()))
        );
    }
    /// The steps in reconcile order, a move to the machine and back: start, the target's
    /// evidence, focus, commit, release, with what each sends and what is shown and viewed
    /// after it.
    #[test]
    fn each_step_of_a_move_there_and_back_acts_on_the_right_connection() {
        let mut f = Views::new();
        for returning in [false, true] {
            let (to, from) = if returning {
                (ClientEndpointId::Local, remote())
            } else {
                (remote(), ClientEndpointId::Local)
            };
            let (to_sent, from_sent) = if returning {
                (f.local.clone(), f.target.clone())
            } else {
                (f.target.clone(), f.local.clone())
            };
            to_sent.take();
            from_sent.take();
            assert!(f.registry.viewed(&from));
            f.pick(to.clone());
            assert_eq!(f.start(), StartOutcome::Started);
            assert!(from_sent.take().is_empty());
            let messages = to_sent.take();
            assert!(matches!(
                messages.first(),
                Some(ClientMessage::ClientShellResize { .. })
            ));
            let Some(ClientMessage::ClientShellEndpointRequest {
                boot_id,
                request_id,
                command:
                    EndpointCommand::ClientShellSurfaceSet(ClientShellSurfaceSetParams { active: true }),
            }) = messages.last()
            else {
                panic!("expected the view request last: {messages:?}");
            };
            assert_eq!(boot_id, &boot(&to));
            assert!(
                !messages
                    .iter()
                    .any(|m| matches!(m, ClientMessage::ClientShellFocus { .. }))
            );
            assert!(f.registry.viewed(&to));
            f.evidence_for(&to, request_id, if returning { 3 } else { 2 });
            assert!(to_sent.take().is_empty());
            assert!(
                f.shell
                    .endpoints
                    .choice
                    .preparing()
                    .expect("preparing")
                    .ready()
                    .is_some()
            );
            send_focus(&mut f.shell.endpoints.choice, &mut f.registry);
            assert!(
                commit_move(&mut f.registry, &mut f.shell, true)
                    .expect("commit")
                    .is_some()
            );
            assert_eq!(f.shell.endpoints.choice.presented(), &to);
            assert!(f.shell.endpoint_is_active(&to));
            assert!(f.registry.viewed(&to));
            assert!(matches!(
                to_sent.take().as_slice(),
                [
                    ClientMessage::ClientShellFocus { focused: true },
                    ClientMessage::ReplayHostEffects
                ]
            ));
            assert_eq!(
                release_unwanted(&f.shell.endpoints.choice, &mut f.registry, &f.shell),
                1
            );
            let released = from_sent.take();
            assert!(
                matches!(
                    released.as_slice(),
                    [
                        ClientMessage::ClientShellFocus { focused: false },
                        ClientMessage::ClientShellEndpointRequest {
                            boot_id,
                            command: EndpointCommand::ClientShellSurfaceSet(
                                ClientShellSurfaceSetParams { active: false }
                            ),
                            ..
                        }
                    ] if boot_id == &boot(&from)
                ),
                "{released:?}"
            );
            assert!(!f.registry.viewed(&from));
            assert!(f.registry.viewed(&to));
        }
    }
}
