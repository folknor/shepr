//! The client's side of every endpoint: connections, command lanes, reconnect supervisors
//! and the move between presentations.

use super::commands::{EndpointCommandCancellation, EndpointCommandResult, EndpointCommands};
use super::view::{self, HostBaseline, StartOutcome};
use super::{
    ClientEndpointId, ClientEndpointStatus, ConnectionRole, EndpointChoice, EndpointConnectOptions,
    EndpointFailureStatus, EndpointRegistry, EndpointSupervisorEvent, EndpointSupervisors,
    EndpointTransportFailure, LocalFailurePolicy, Lost, PresentationDecision, PresentationGate,
    Selection,
};
use crate::errors::LoopExit;
use crate::events::ClientLoopEvent;
use crate::shell::{self, ClientShellState};
use shepr_protocol::command::{EndpointError, EndpointReply};
use shepr_protocol::{
    BootId, ClientMessage, ClientShellSnapshot, ConnectionGeneration, RequestId, TerminalGeometry,
};
use shepr_surface::decode::{DecodedClientServerMessage, DecodedWireServerMessage};
use std::collections::VecDeque;
use std::io;
use std::time::Instant;
use tracing::warn;

/// The client's side of every endpoint: connections, command lanes, reconnect supervisors,
/// and the move between presentations. Connections, supervisors and command lanes serve
/// every endpoint, hidden ones included; the shell's endpoint choice decides only what is
/// shown. This is the only production code that transitions the endpoint choice; the shell
/// reads what is presented and never moves it.
pub(crate) struct EndpointHub {
    registry: EndpointRegistry,
    commands: EndpointCommands,
    supervisors: EndpointSupervisors,
    local_failure_policy: LocalFailurePolicy,
}

/// What a reconcile or a supervisor event asks the loop to do to the host, in order.
pub(crate) enum HubEffect {
    /// Show this endpoint notice and present the chrome.
    Notice(shell::EndpointNotice),
    /// Drop the host modes, title and report-all a lost or retired endpoint requested.
    ClearHostEffects,
    ChromeDirty,
    /// A move committed: discard the blit baseline and compose the pane.
    Committed,
}

/// How one inbound message relates to the presentation.
pub(crate) enum Admission {
    /// A stale generation, a role the gate drops, or move evidence the hub kept.
    Consumed,
    /// For the loop to apply. A snapshot from the move's target was also kept as evidence.
    Present {
        message: Box<DecodedClientServerMessage>,
        role: ConnectionRole,
    },
}

/// What installing a snapshot changed on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotDirty {
    /// The shown endpoint's projection moved.
    Pane,
    /// Another endpoint's snapshot was cached: only the machine list can differ.
    Chrome,
}

pub(crate) struct Dispatched {
    pub(crate) repaint: shell::Repaint,
    /// Clipboard writes, in action order, for the loop to forward to the host.
    pub(crate) clipboard: Vec<Vec<u8>>,
}

impl EndpointHub {
    pub(crate) fn new(
        registry: EndpointRegistry,
        supervisors: EndpointSupervisors,
        local_failure_policy: LocalFailurePolicy,
    ) -> Self {
        Self {
            registry,
            commands: EndpointCommands::default(),
            supervisors,
            local_failure_policy,
        }
    }

    // Loop timing and connections

    /// The earliest deadline the hub itself has: the move's, the command lanes', the
    /// connections' health and the supervisors' next attempt.
    pub(crate) fn next_deadline(
        &mut self,
        shell: &ClientShellState,
        now: Instant,
    ) -> Option<Instant> {
        [
            shell.endpoints.choice.deadline(),
            self.commands.next_deadline(),
            self.registry.next_service_deadline(now),
            self.supervisors.next_retry_deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Starts every connection attempt that is due.
    pub(crate) fn spawn_due(
        &mut self,
        now: Instant,
        options: impl Fn() -> EndpointConnectOptions,
        events: &tokio::sync::mpsc::Sender<ClientLoopEvent>,
    ) {
        self.supervisors.spawn_due(now, options, events);
    }

    /// Applies one supervisor outcome: a failed attempt's status, or a new connection.
    pub(crate) fn supervisor_event(
        &mut self,
        shell: &mut ClientShellState,
        event: EndpointSupervisorEvent,
        now: Instant,
    ) -> Vec<HubEffect> {
        match event {
            EndpointSupervisorEvent::Status {
                endpoint_id,
                generation,
                status,
                message,
                connector,
            } => {
                self.supervisors
                    .return_connector(&endpoint_id, generation, connector);
                if !self
                    .supervisors
                    .record_status(&endpoint_id, generation, status.into(), now)
                {
                    return Vec::new();
                }
                if status == EndpointFailureStatus::Attention {
                    warn!(endpoint = %endpoint_id, %generation, error = %message, "endpoint needs attention");
                }
                shell.set_endpoint_status(&endpoint_id, status);
                shell.set_machine_diagnostic(&endpoint_id, &message);
                // Handshake diagnostics carry only the failing phase; the status line
                // supplies the configured endpoint label once.
                let unavailable = (status == EndpointFailureStatus::Attention
                    && shell.endpoint_is_active(&endpoint_id))
                .then(|| {
                    shell::EndpointNotice::new(
                        endpoint_id.clone(),
                        shell::EndpointNoticeKind::StatusFailure(message.to_string()),
                    )
                });
                vec![unavailable.map_or(HubEffect::ChromeDirty, HubEffect::Notice)]
            }
            EndpointSupervisorEvent::Connected {
                endpoint_id,
                generation,
                connection,
                connector,
            } => {
                self.supervisors
                    .return_connector(&endpoint_id, generation, connector);
                if !self.supervisors.record_status(
                    &endpoint_id,
                    generation,
                    ClientEndpointStatus::Online,
                    now,
                ) {
                    return Vec::new();
                }
                self.registry.insert_native(
                    endpoint_id.clone(),
                    connection.activate(),
                    generation,
                    false,
                    now,
                );
                shell.endpoint_connected(&endpoint_id, generation);
                // Connecting changes no pane projection (the connection has no surface
                // yet), only the machine list.
                vec![HubEffect::ChromeDirty]
            }
        }
    }

    /// A reader reported its connection closed. A generation the registry no longer accepts
    /// is a connection already replaced or failed, and changes nothing.
    pub(crate) fn disconnected(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: ConnectionGeneration,
        error: &io::Error,
    ) {
        if self.registry.accepts(endpoint_id, generation) {
            self.registry.fail(endpoint_id, error);
        }
    }

    /// Records a failure of the endpoint's connection; `reconcile` handles it as a loss.
    pub(crate) fn fail(&mut self, endpoint_id: &ClientEndpointId, error: &io::Error) {
        self.registry.fail(endpoint_id, error);
    }

    /// Whether losing this endpoint's connection ends the client.
    pub(crate) fn ends_client_for(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.local_failure_policy
            .ends_client_for(endpoint_id.policy())
    }

    pub(crate) fn tick_health(&mut self, now: Instant) {
        self.registry.tick_health(now);
    }

    /// Settles every in-flight endpoint command whose deadline has passed, each exactly
    /// once. `expire` takes the command out of its lane, so whatever is not answered here is
    /// never reported by a later lane disconnect. A command whose connection is gone is
    /// dropped as interrupted: the timer tick that expires it can first record a failed
    /// health check, which removes the connection before the next reconcile disconnects the
    /// lane. Only a command still on its own connection is answered with its timeout.
    pub(crate) fn settle_expired(
        &mut self,
        shell: &mut ClientShellState,
        now: Instant,
    ) -> shell::ClientShellInput {
        let mut outcome = shell::ClientShellInput::default();
        for expired in self.commands.expire(now) {
            if !self
                .registry
                .accepts(&expired.endpoint_id, expired.generation)
            {
                outcome.repaint |= shell
                    .drop_request(&expired.request_id, shell::DropReason::Interrupted)
                    .is_needed();
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

    // The move

    /// The one derivation of what is shown and which connections are viewed, run once per
    /// loop iteration before the loop waits: queued connection failures, a failed or expired
    /// move, starting a move, the move's navigation, its commit, and turning off every
    /// viewed connection nobody wants. Failures come first, so a target lost at its deadline
    /// reads as an interrupted switch rather than a timeout. The effects on the host are
    /// returned in order for the loop to apply. A failure the local policy ends the client
    /// for returns `Err` at once.
    pub(crate) fn reconcile<'t>(
        &mut self,
        shell: &mut ClientShellState,
        baseline: impl FnOnce(&ClientShellState) -> HostBaseline<'t>,
        now: Instant,
    ) -> Result<Vec<HubEffect>, LoopExit> {
        let mut effects = Vec::new();
        for failure in self.registry.take_failures() {
            // `record_failure` removed the connection as it queued this failure, and no
            // connection of another generation can have replaced it yet: a connection is only
            // installed by its supervisor's attempt, the supervisor starts no attempt while
            // its generation is connected, and only `endpoint_lost` below re-arms it. So
            // every queued failure ends its endpoint's lane here.
            warn!(
                endpoint = %failure.endpoint_id,
                error = %failure.failure,
                "endpoint transport failed"
            );
            if self.ends_client_for(&failure.endpoint_id) {
                return Err(LoopExit::ConnectionLost(io::Error::new(
                    failure.kind,
                    failure,
                )));
            }
            self.endpoint_lost(shell, &failure, now, &mut effects);
        }
        if let Some(preparing) = shell.endpoints.choice.preparing() {
            if let Some(rejection) = preparing.rejection() {
                let rejection = rejection.to_string();
                fail_move(
                    shell,
                    shell::EndpointNoticeKind::MoveRejected(rejection),
                    &mut effects,
                );
            } else if now >= preparing.deadline() {
                fail_move(
                    shell,
                    shell::EndpointNoticeKind::MoveSurfaceTimedOut,
                    &mut effects,
                );
            }
        }
        // Focus is sent at commit. Geometry and theme are used only if a move is ready to
        // start; deriving the layout on every ordinary event would repeat shell work.
        let focused = shell.host_focus_baseline();
        if let StartOutcome::Abandoned(to) =
            view::start_move(&mut self.registry, shell, baseline, now)
        {
            effects.push(HubEffect::Notice(shell::EndpointNotice::new(
                to,
                shell::EndpointNoticeKind::NotReady,
            )));
        }
        view::send_focus(&mut shell.endpoints.choice, &mut self.registry);
        match view::commit_move(&mut self.registry, shell, focused) {
            Ok(Some(committed)) => {
                if let Some(previous) = committed.previous {
                    effects.push(HubEffect::ClearHostEffects);
                    let cancelled = self.commands.retire_lane(&previous);
                    if cancel_commands(shell, cancelled).is_needed() {
                        effects.push(HubEffect::ChromeDirty);
                    }
                }
                let cancelled = self
                    .commands
                    .send_next(&committed.shown, &mut self.registry, now);
                if cancel_commands(shell, cancelled).is_needed() {
                    effects.push(HubEffect::ChromeDirty);
                }
                effects.push(HubEffect::Committed);
            }
            Err(reason) => fail_move(
                shell,
                shell::EndpointNoticeKind::MoveRejected(reason.to_string()),
                &mut effects,
            ),
            Ok(None) => {}
        }
        view::release_unwanted(&shell.endpoints.choice, &mut self.registry, shell);
        Ok(effects)
    }

    /// Handles one lost connection: supervisor and diagnostic, the choice, its command lane,
    /// the shell status, then the notice for what the loss meant to the choice.
    fn endpoint_lost(
        &mut self,
        shell: &mut ClientShellState,
        failure: &EndpointTransportFailure,
        now: Instant,
        effects: &mut Vec<HubEffect>,
    ) {
        let id = &failure.endpoint_id;
        let notice = failure.failure.disconnect_notice();
        let status = EndpointFailureStatus::after_failure(&failure.failure);
        self.supervisors
            .record_status(id, failure.generation, status.into(), now);
        shell.set_machine_diagnostic(id, &failure.failure);
        let (lost, cancellation_repaint) = self.requests_lost(shell, id, status);
        if cancellation_repaint.is_needed() {
            effects.push(HubEffect::ChromeDirty);
        }
        // The disconnect predicate is fixed UI text; remote diagnostics stay in machine
        // diagnostics, and every raw transport error stays in the log.
        match lost {
            Lost::Shown => {
                effects.push(HubEffect::Notice(shell::EndpointNotice::new(
                    id.clone(),
                    shell::EndpointNoticeKind::ConnectionLost(notice),
                )));
                effects.push(HubEffect::ClearHostEffects);
            }
            Lost::Target => {
                effects.push(HubEffect::Notice(shell::EndpointNotice::new(
                    id.clone(),
                    shell::EndpointNoticeKind::MoveInterrupted(notice),
                )));
            }
            Lost::Unrelated => effects.push(HubEffect::ChromeDirty),
        }
    }

    /// How one inbound message relates to the presentation. The generation check and the
    /// presentation gate precede every message effect, including endpoint requests to
    /// change host modes or write the clipboard. Evidence for the move being prepared is
    /// kept here, and a target's snapshot is both kept and passed on.
    pub(crate) fn admit(
        &mut self,
        shell: &mut ClientShellState,
        endpoint_id: &ClientEndpointId,
        generation: ConnectionGeneration,
        message: Box<DecodedClientServerMessage>,
    ) -> Admission {
        if !self.registry.accepts(endpoint_id, generation) {
            return Admission::Consumed;
        }
        let role = shell.endpoints.choice.role(endpoint_id);
        let move_response = match message.as_ref() {
            DecodedClientServerMessage::Wire(
                DecodedWireServerMessage::ClientShellEndpointResponse {
                    boot_id,
                    request_id,
                    ..
                },
            ) => shell.endpoints.choice.preparing().is_some_and(|preparing| {
                preparing.accepts_response(endpoint_id, generation, boot_id, request_id)
            }),
            _ => false,
        };
        match PresentationGate::new(role, move_response).decide(message.as_ref()) {
            PresentationDecision::Drop => Admission::Consumed,
            PresentationDecision::Apply => Admission::Present { message, role },
            PresentationDecision::ApplyAndBuffer => {
                if let DecodedClientServerMessage::Wire(DecodedWireServerMessage::EndpointSnapshot(
                    snapshot,
                )) = message.as_ref()
                    && let Some(pending) = shell.endpoints.choice.preparing_mut()
                {
                    pending.receive_snapshot(endpoint_id, generation, snapshot);
                }
                Admission::Present { message, role }
            }
            PresentationDecision::Buffer => {
                if let Some(pending) = shell.endpoints.choice.preparing_mut() {
                    match *message {
                        DecodedClientServerMessage::PaneSurfacePatch(patch) => {
                            pending.receive_patch(endpoint_id, generation, &patch);
                        }
                        DecodedClientServerMessage::Wire(
                            DecodedWireServerMessage::PaneSurface(surface),
                        ) => {
                            pending.receive_surface(endpoint_id, generation, surface);
                        }
                        DecodedClientServerMessage::Wire(
                            DecodedWireServerMessage::ClientShellEndpointResponse {
                                boot_id,
                                request_id,
                                result,
                            },
                        ) => {
                            pending.receive_response(
                                endpoint_id,
                                generation,
                                &boot_id,
                                &request_id,
                                result,
                            );
                        }
                        DecodedClientServerMessage::Wire(_) => {}
                    }
                }
                Admission::Consumed
            }
        }
    }

    /// Completes only the in-flight command a response answers; the lane ignores retired,
    /// expired and unknown requests.
    pub(crate) fn complete_command(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: ConnectionGeneration,
        boot_id: &BootId,
        request_id: &RequestId,
        result: Result<EndpointReply, EndpointError>,
    ) -> Option<EndpointCommandResult> {
        self.commands
            .receive_response(endpoint_id, generation, boot_id, request_id, result)
    }

    /// Installs a snapshot of the connection's current generation and marks the connection
    /// ready. Only the shown endpoint's snapshot moves the projection; any other one (a
    /// target being prepared included) is cached for its commit. Snapshot application only
    /// moves Copy and Terminal modes; neither asks the host for report-all keys. `None`:
    /// the connection is gone.
    pub(crate) fn install_snapshot(
        &mut self,
        shell: &mut ClientShellState,
        endpoint_id: &ClientEndpointId,
        snapshot: Box<ClientShellSnapshot>,
        role: ConnectionRole,
    ) -> Option<SnapshotDirty> {
        let generation = self.registry.connection(endpoint_id)?.generation;
        let dirty = if role == ConnectionRole::Shown {
            shell.set_endpoint_snapshot_for_generation(endpoint_id, generation, snapshot);
            SnapshotDirty::Pane
        } else {
            shell.cache_endpoint_snapshot_for_generation(endpoint_id, generation, snapshot);
            SnapshotDirty::Chrome
        };
        self.registry.mark_ready(endpoint_id, generation);
        Some(dirty)
    }

    /// Ends every request of a lost connection. The choice learns of the loss first. The
    /// lane's account comes next: a command still queued behind the in-flight one was never
    /// sent and drops as unsent, the in-flight one as interrupted. Only then is the endpoint
    /// marked failed, whose blanket interruption covers anything the lane did not hold.
    pub(crate) fn requests_lost(
        &mut self,
        shell: &mut ClientShellState,
        endpoint_id: &ClientEndpointId,
        status: EndpointFailureStatus,
    ) -> (Lost, shell::Repaint) {
        let lost = shell.endpoints.choice.connection_lost(endpoint_id);
        let cancelled = self.commands.disconnect(endpoint_id);
        let repaint = cancel_commands(shell, cancelled);
        shell.endpoint_failed(endpoint_id, status);
        (lost, repaint)
    }

    // Shell output

    /// Carries out what the shell asked of the endpoints, in order: endpoint commands
    /// enter the shown endpoint's lane, a pick moves the choice, and clipboard writes are
    /// returned for the loop to forward to the host.
    pub(crate) fn dispatch(
        &mut self,
        shell: &mut ClientShellState,
        actions: Vec<shell::ClientShellAction>,
        now: Instant,
    ) -> Dispatched {
        let mut repaint = shell::Repaint::Unchanged;
        let mut clipboard = Vec::new();
        let mut actions = VecDeque::from(actions);
        while let Some(action) = actions.pop_front() {
            match action {
                shell::ClientShellAction::Endpoint {
                    endpoint_id,
                    boot_id,
                    request,
                } => {
                    if input_endpoint(&shell.endpoints.choice, &self.registry) == Some(&endpoint_id)
                        && let Some(connection) = self.registry.connection(&endpoint_id)
                    {
                        // A superseded split ratio leaves the queue unsent.
                        let superseded = self.commands.enqueue(
                            endpoint_id,
                            connection.generation,
                            boot_id,
                            request,
                        );
                        repaint |= cancel_commands(shell, superseded);
                    } else {
                        // This action has not entered the endpoint send queue, so its outcome
                        // is known locally and must not be presented as an interrupted server
                        // action.
                        repaint |= shell.drop_request(&request.id, shell::DropReason::Unsent);
                    }
                }
                shell::ClientShellAction::ClipboardWrite(bytes) => clipboard.push(bytes),
                shell::ClientShellAction::ActivateEndpoint(destination) => {
                    let endpoint_id = destination.endpoint.clone();
                    match shell.endpoints.choice.select(destination) {
                        Selection::Unchanged => {}
                        Selection::FocusShown(target) => {
                            actions.extend(shell.focus_endpoint_target(target));
                            repaint = shell::Repaint::Needed;
                        }
                        Selection::Moving => {
                            if view::selection_wait_notice_needed(
                                &endpoint_id,
                                &shell.endpoints.choice,
                                &self.registry,
                                shell,
                            ) {
                                let notice = waiting_notice(
                                    endpoint_id.clone(),
                                    shell.endpoint_status(&endpoint_id),
                                );
                                if shell.receive_endpoint_unavailable(&notice) {
                                    repaint = shell::Repaint::Needed;
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(shown) = input_endpoint(&shell.endpoints.choice, &self.registry) {
            let cancelled = self.commands.send_next(shown, &mut self.registry, now);
            repaint |= cancel_commands(shell, cancelled);
        }
        Dispatched { repaint, clipboard }
    }

    /// Sends pane input or host focus to the shown endpoint, while its connection exists
    /// and is viewed. A target learns of host focus at its commit, and a released endpoint
    /// was sent focus-loss with its release.
    pub(crate) fn send_shown(&mut self, shell: &ClientShellState, message: &ClientMessage) {
        if let Some(shown) = input_endpoint(&shell.endpoints.choice, &self.registry) {
            self.registry.send_to(shown, message);
        }
    }

    /// Sends to every viewed connection: the shown endpoint and a target being prepared.
    pub(crate) fn send_viewed(&mut self, message: &ClientMessage) {
        self.registry.send_viewed(message);
    }

    /// Sends `geometry` to every viewed connection. A changed geometry also drops the
    /// move's recorded surface; an unchanged one keeps it, because the server answers an
    /// unchanged resize with no new surface.
    pub(crate) fn resize_views(
        &mut self,
        shell: &mut ClientShellState,
        geometry: TerminalGeometry,
    ) {
        if let Some(preparing) = shell.endpoints.choice.preparing_mut() {
            preparing.update_geometry(geometry);
        }
        self.registry
            .send_viewed(&ClientMessage::ClientShellResize { geometry });
    }

    /// Tells the shown endpoint this client is leaving. A failed send is recorded against
    /// the endpoint. The registry remembers a sent Detach, so its Drop on the way out only
    /// flushes this connection.
    pub(crate) fn detach(&mut self, shell: &ClientShellState) {
        if let Some(shown) = shell.endpoints.choice.live() {
            self.registry.send_to(shown, &ClientMessage::Detach);
        }
    }
}

/// Where pane input and endpoint commands go: the shown endpoint, while its connection exists
/// and is viewed. During a move that is the source, which stays live until the commit.
fn input_endpoint<'a>(
    choice: &'a EndpointChoice,
    registry: &EndpointRegistry,
) -> Option<&'a ClientEndpointId> {
    choice.live().filter(|id| registry.viewed(id))
}

/// Fails the move being prepared and reports `notice` against its target. The target needs no
/// cleanup: it is no longer wanted, so the release step of the same reconcile turns it off.
fn fail_move(
    shell: &mut ClientShellState,
    notice: shell::EndpointNoticeKind,
    effects: &mut Vec<HubEffect>,
) {
    if let Some(failed) = shell.endpoints.choice.fail_move() {
        effects.push(HubEffect::Notice(shell::EndpointNotice::new(
            failed.to, notice,
        )));
    }
}

fn cancel_commands(
    shell: &mut ClientShellState,
    cancelled: EndpointCommandCancellation,
) -> shell::Repaint {
    let mut repaint = shell::Repaint::Unchanged;
    for request_id in cancelled.unsent {
        repaint |= shell.drop_request(&request_id, shell::DropReason::Unsent);
    }
    for request_id in cancelled.possibly_sent {
        repaint |= shell.drop_request(&request_id, shell::DropReason::Interrupted);
    }
    repaint
}

/// The notice for a pick that has to wait for its endpoint's connection or metadata. It names
/// the current status, so an attention diagnostic does not read like a promise that waiting
/// will repair it.
fn waiting_notice(
    endpoint: ClientEndpointId,
    status: Option<ClientEndpointStatus>,
) -> shell::EndpointNotice {
    shell::EndpointNotice::new(
        endpoint,
        shell::EndpointNoticeKind::WaitingForSelection(status),
    )
}

#[cfg(test)]
impl EndpointHub {
    /// A hub over `registry` alone: no supervisors, and a lost Local connection is
    /// survivable, as with configured machines.
    pub(crate) fn for_registry(registry: EndpointRegistry) -> Self {
        let supervisors = EndpointSupervisors::new(Vec::new(), Instant::now())
            .expect("test precondition: no configured supervisors");
        Self::new(registry, supervisors, LocalFailurePolicy::Reconnect)
    }

    pub(crate) fn registry(&self) -> &EndpointRegistry {
        &self.registry
    }

    pub(crate) fn registry_mut(&mut self) -> &mut EndpointRegistry {
        &mut self.registry
    }

    pub(crate) fn commands_mut(&mut self) -> &mut EndpointCommands {
        &mut self.commands
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::tests::{pending_request, read_then_command};
    use crate::shell::{ClientShellConfig, Location, LocationTarget};
    use crate::tests::endpoints::RecordingTransport;
    use crate::tests::test_generation;
    use shepr_test_fixtures::ValidatedClientConfigFixture as _;

    /// A hub whose only connection is Local, at generation 1.
    fn local_hub(local: RecordingTransport) -> EndpointHub {
        EndpointHub::for_registry(EndpointRegistry::new(local, test_generation(1)))
    }

    fn shell_with(choice: EndpointChoice) -> ClientShellState {
        let mut shell = ClientShellState::new(ClientShellConfig::from_validated_config(
            &shepr_config::ValidatedClientConfig::test_default(),
        ));
        shell.endpoints.choice = choice;
        shell
    }

    #[test]
    fn a_pick_is_applied_at_once_without_an_event_round_trip() {
        let mut hub = local_hub(RecordingTransport::default());
        let mut shell = shell_with(EndpointChoice::waiting_for(ClientEndpointId::Local));
        hub.dispatch(
            &mut shell,
            vec![shell::ClientShellAction::ActivateEndpoint(Location {
                endpoint: ClientEndpointId::Local,
                target: LocationTarget::Workspace(shepr_test_fixtures::id("w1")),
            })],
            Instant::now(),
        );
        assert_eq!(
            shell
                .endpoints
                .choice
                .pending_start()
                .expect("waiting pick")
                .to,
            &ClientEndpointId::Local
        );
        assert!(shell.endpoints.choice.live().is_none());
    }

    #[test]
    fn selecting_the_shown_endpoint_is_a_noop_but_with_nothing_shown_it_reproves() {
        for shown in [false, true] {
            let mut hub = local_hub(RecordingTransport::default());
            let choice = if shown {
                EndpointChoice::showing(ClientEndpointId::Local)
            } else {
                // Nothing shown, and a proof of Local already failed on this generation: only an
                // explicit pick may retry it there.
                let mut choice = EndpointChoice::waiting_for(ClientEndpointId::Local);
                choice.begin_preparing(
                    super::super::ViewLease {
                        endpoint_id: ClientEndpointId::Local,
                        generation: test_generation(1),
                        boot_id: crate::tests::test_boot_id("boot-1"),
                        minimum_revision: shepr_protocol::ProjectionRevision::FIRST,
                    },
                    RequestId::allocate(),
                    TerminalGeometry::from_host(
                        shepr_core::geometry::GridSize::clamped(80, 24),
                        shepr_core::geometry::HostCell::from_host(8, 16, false),
                    ),
                    Instant::now(),
                );
                choice.fail_move();
                assert_eq!(
                    choice
                        .pending_start()
                        .expect("failed proof")
                        .failed_generation,
                    Some(test_generation(1))
                );
                choice
            };
            let mut shell = shell_with(choice);
            hub.dispatch(
                &mut shell,
                vec![shell::ClientShellAction::ActivateEndpoint(
                    Location::machine(ClientEndpointId::Local),
                )],
                Instant::now(),
            );
            if shown {
                assert!(shell.endpoints.choice.pending_start().is_none());
                assert_eq!(
                    shell.endpoints.choice.live(),
                    Some(&ClientEndpointId::Local)
                );
            } else {
                assert_eq!(
                    shell
                        .endpoints
                        .choice
                        .pending_start()
                        .expect("rearmed proof")
                        .failed_generation,
                    None
                );
            }
        }
    }

    #[test]
    fn dispatcher_cancels_pending_requests_on_an_unviewed_endpoint_or_failed_send() {
        for fail_send in [false, true] {
            let (mut state, actions) = pending_request();
            let local = RecordingTransport::default();
            if fail_send {
                local.fail_next();
            }
            let mut endpoints = EndpointRegistry::new(local, test_generation(1));
            endpoints.set_viewed(&ClientEndpointId::Local, fail_send);
            let mut hub = EndpointHub::for_registry(endpoints);
            let dispatched = hub.dispatch(&mut state, actions, Instant::now());
            assert!(dispatched.repaint.is_needed());
            assert!(!state.has_open_requests());
            // A request refused before it entered the send queue has a known
            // outcome and is not reported as interrupted; one whose send failed
            // may have reached the server.
            assert_eq!(
                state.visible_notice_title() == Some("Action interrupted"),
                fail_send
            );
            assert_eq!(
                hub.commands_mut().disconnect(&ClientEndpointId::Local),
                EndpointCommandCancellation::default()
            );
        }
    }

    #[test]
    fn an_expired_command_is_settled_even_when_its_connection_was_lost_first() {
        for connection_lost in [false, true] {
            let (mut state, actions) = pending_request();
            let mut hub = local_hub(RecordingTransport::default());
            // The dispatch enqueues the request in the shown endpoint's lane and sends it.
            let sent_at = Instant::now();
            let dispatched = hub.dispatch(&mut state, actions, sent_at);
            assert!(!dispatched.repaint.is_needed());
            assert!(state.has_open_requests());
            if connection_lost {
                // A failed health check on the same timer tick removes the connection before the
                // command expires; the lane disconnect comes only with the next reconcile.
                hub.registry_mut().fail(
                    &ClientEndpointId::Local,
                    &io::Error::new(io::ErrorKind::TimedOut, "health check timed out"),
                );
            }

            let outcome = hub.settle_expired(
                &mut state,
                sent_at + crate::limits::ENDPOINT_COMMAND_TIMEOUT,
            );

            assert!(outcome.repaint);
            assert!(!state.has_open_requests());
            let expected = if connection_lost {
                "Action interrupted"
            } else {
                "Server timed out"
            };
            assert_eq!(state.visible_notice_title(), Some(expected));
            assert_eq!(
                hub.commands_mut().disconnect(&ClientEndpointId::Local),
                EndpointCommandCancellation::default()
            );
        }
    }

    #[test]
    fn a_lost_endpoint_drops_its_queued_commands_as_unsent() {
        // A command queued behind the in-flight one was never sent: losing the endpoint drops it
        // as unsent, so no interrupted-action notice appears.
        let (mut s, actions) = read_then_command();
        let mut hub = local_hub(RecordingTransport::default());
        // The first request is sent and the second waits behind it.
        let dispatched = hub.dispatch(&mut s, actions, Instant::now());
        assert!(!dispatched.repaint.is_needed());
        assert!(s.has_open_requests());

        hub.requests_lost(
            &mut s,
            &ClientEndpointId::Local,
            EndpointFailureStatus::Reconnecting,
        );
        assert!(!s.has_open_requests());
        assert_eq!(s.visible_notice_title(), None);

        // A lone command that was sent may have reached the server: it is interrupted.
        let (mut state, actions) = pending_request();
        let mut hub = local_hub(RecordingTransport::default());
        hub.dispatch(&mut state, actions, Instant::now());
        hub.requests_lost(
            &mut state,
            &ClientEndpointId::Local,
            EndpointFailureStatus::Reconnecting,
        );
        assert!(!state.has_open_requests());
        assert_eq!(state.visible_notice_title(), Some("Action interrupted"));
    }

    /// A full pane surface and a patch from Local, at the connection's own generation.
    fn pane_messages() -> [DecodedClientServerMessage; 2] {
        let surface = shepr_protocol::PaneSurfaceFrame {
            boot_id: crate::tests::test_boot_id("boot-1"),
            projection_revision: shepr_protocol::ProjectionRevision::FIRST,
            surface_revision: shepr_protocol::SurfaceRevision::FIRST,
            frame: shepr_protocol::FrameData::blank(80, 24).expect("test frame size is valid"),
            panes: vec![],
            splits: vec![],
        };
        let patch = shepr_protocol::PaneSurfacePatch {
            boot_id: surface.boot_id.clone(),
            projection_revision: shepr_protocol::ProjectionRevision::FIRST,
            base_surface_revision: shepr_protocol::SurfaceRevision::FIRST,
            surface_revision: shepr_test_fixtures::counter_at(2),
            rows: vec![],
            panes: vec![],
            cursor: None,
        };
        [
            DecodedClientServerMessage::Wire(DecodedWireServerMessage::PaneSurface(surface)),
            DecodedClientServerMessage::PaneSurfacePatch(patch),
        ]
    }

    fn local_lease() -> super::super::ViewLease {
        super::super::ViewLease {
            endpoint_id: ClientEndpointId::Local,
            generation: test_generation(1),
            boot_id: crate::tests::test_boot_id("boot-1"),
            minimum_revision: shepr_protocol::ProjectionRevision::FIRST,
        }
    }

    fn geometry() -> TerminalGeometry {
        TerminalGeometry::from_host(
            shepr_core::geometry::GridSize::clamped(80, 24),
            shepr_core::geometry::HostCell::from_host(8, 16, false),
        )
    }

    /// `present_frame` always writes, which is sound only because the gate never passes a
    /// pane surface or patch to the loop while no endpoint is live: whatever the move's
    /// stage, the cells on screen stay the last coherent ones until a commit installs the
    /// target's pair.
    #[test]
    fn with_no_endpoint_shown_the_gate_passes_no_pane_frame() {
        let remote = ClientEndpointId::Ssh(
            shepr_config::MachineLabel::parse("build").expect("machine label"),
        );
        let nothing_shown = [
            // Launch with Local unreachable: Local is waited for, not shown.
            EndpointChoice::waiting_for(ClientEndpointId::Local),
            // Local is the target being prepared, with nothing shown behind it.
            {
                let mut choice = EndpointChoice::waiting_for(ClientEndpointId::Local);
                choice.begin_preparing(
                    local_lease(),
                    RequestId::allocate(),
                    geometry(),
                    Instant::now(),
                );
                choice
            },
            // Preparing Local failed on this generation.
            {
                let mut choice = EndpointChoice::waiting_for(ClientEndpointId::Local);
                choice.begin_preparing(
                    local_lease(),
                    RequestId::allocate(),
                    geometry(),
                    Instant::now(),
                );
                choice.fail_move();
                choice
            },
            // Local was shown, a move to another machine was preparing, and Local's
            // connection was lost: Local's cells stay drawn as a stale presentation.
            {
                let mut choice = EndpointChoice::showing(ClientEndpointId::Local);
                choice.select(Location::machine(remote.clone()));
                choice.begin_preparing(
                    super::super::ViewLease {
                        endpoint_id: remote.clone(),
                        generation: test_generation(7),
                        boot_id: crate::tests::test_boot_id("remote-boot"),
                        minimum_revision: shepr_protocol::ProjectionRevision::FIRST,
                    },
                    RequestId::allocate(),
                    geometry(),
                    Instant::now(),
                );
                assert_eq!(
                    choice.connection_lost(&ClientEndpointId::Local),
                    Lost::Shown
                );
                assert_eq!(choice.presented(), &ClientEndpointId::Local);
                choice
            },
        ];
        for (case, choice) in nothing_shown.into_iter().enumerate() {
            assert!(choice.live().is_none(), "case {case}: nothing is shown");
            let mut hub = local_hub(RecordingTransport::default());
            let mut shell = shell_with(choice);
            for message in pane_messages() {
                let admission = hub.admit(
                    &mut shell,
                    &ClientEndpointId::Local,
                    test_generation(1),
                    Box::new(message),
                );
                assert!(
                    matches!(admission, Admission::Consumed),
                    "case {case}: a pane frame passed the gate with nothing shown"
                );
            }
        }

        // The same frames from the shown endpoint do pass, so the cases above are
        // refused by the gate and not by the generation check.
        let mut hub = local_hub(RecordingTransport::default());
        let mut shell = shell_with(EndpointChoice::showing(ClientEndpointId::Local));
        for message in pane_messages() {
            let admission = hub.admit(
                &mut shell,
                &ClientEndpointId::Local,
                test_generation(1),
                Box::new(message),
            );
            assert!(matches!(
                admission,
                Admission::Present {
                    role: ConnectionRole::Shown,
                    ..
                }
            ));
        }
    }

    #[test]
    fn waiting_notice_names_the_endpoint_and_its_current_status() {
        let local = ClientEndpointId::Local;
        let build = ClientEndpointId::Ssh(
            shepr_config::MachineLabel::parse("build").expect("machine label"),
        );
        assert_eq!(
            waiting_notice(local, Some(ClientEndpointStatus::Attention)).body(),
            "Local needs attention"
        );
        assert_eq!(
            waiting_notice(build.clone(), Some(ClientEndpointStatus::Reconnecting)).body(),
            "build is reconnecting; selection will resume when it is ready"
        );
        assert_eq!(
            waiting_notice(build, Some(ClientEndpointStatus::Online)).body(),
            "build is waiting for its workspace snapshot; selection will resume when it is ready"
        );
    }
}
