use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use shepr_protocol::command::{
    EndpointCommand, EndpointError, EndpointReply, LayoutSetSplitRatioParams,
};
use shepr_protocol::{BootId, ClientMessage, ConnectionGeneration, RequestId};

use super::{ClientEndpointId, EndpointRegistry, EndpointSendOutcome};
use crate::limits::ENDPOINT_COMMAND_TIMEOUT;
use crate::shell::{ClientShellEndpointError, ClientShellEndpointRequest};

struct QueuedCommand {
    generation: ConnectionGeneration,
    boot_id: BootId,
    request: Box<ClientShellEndpointRequest>,
}

struct InFlightCommand {
    key: RequestKey,
    sent_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequestKey {
    generation: ConnectionGeneration,
    boot_id: BootId,
    request_id: RequestId,
}

pub(crate) struct EndpointCommandResult {
    pub(crate) endpoint_id: ClientEndpointId,
    /// The connection generation the command was sent on. A timed-out command is answered
    /// with its timeout only while that connection is still current; otherwise it is
    /// dropped as interrupted.
    pub(crate) generation: ConnectionGeneration,
    pub(crate) boot_id: BootId,
    pub(crate) request_id: RequestId,
    pub(crate) result: Result<EndpointReply, ClientShellEndpointError>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct EndpointCommandCancellation {
    /// Commands rejected before a transport send was attempted.
    pub(crate) unsent: Vec<RequestId>,
    /// Commands that were in flight or whose transport send failed. The server
    /// may have received them, so their cancellation is reported as uncertain.
    pub(crate) possibly_sent: Vec<RequestId>,
}

#[derive(Default)]
struct EndpointCommandLane {
    queued: VecDeque<QueuedCommand>,
    in_flight: Option<InFlightCommand>,
}

#[derive(Default)]
pub(crate) struct EndpointCommands {
    lanes: HashMap<ClientEndpointId, EndpointCommandLane>,
}

impl EndpointCommands {
    /// Queues `request` behind the lane's in-flight command. A split ratio supersedes a ratio
    /// still queued for the same split (same workspace, path and layout epoch) on the same
    /// connection and boot: a drag sends a ratio per step, and on a slow link only the newest
    /// is worth sending, so the older one leaves the queue and is returned as unsent. The new
    /// ratio joins the back of the queue, after everything issued before it. The in-flight
    /// command is never touched.
    pub(crate) fn enqueue(
        &mut self,
        endpoint_id: ClientEndpointId,
        generation: ConnectionGeneration,
        boot_id: BootId,
        request: Box<ClientShellEndpointRequest>,
    ) -> EndpointCommandCancellation {
        let lane = self.lanes.entry(endpoint_id).or_default();
        let mut cancelled = EndpointCommandCancellation::default();
        if let EndpointCommand::LayoutSetSplitRatio(params) = &request.command
            && let Some(index) = lane.queued.iter().position(|queued| {
                queued.generation == generation
                    && queued.boot_id == boot_id
                    && matches!(
                        &queued.request.command,
                        EndpointCommand::LayoutSetSplitRatio(older) if same_split(older, params)
                    )
            })
            && let Some(superseded) = lane.queued.remove(index)
        {
            cancelled.unsent.push(superseded.request.id);
        }
        lane.queued.push_back(QueuedCommand {
            generation,
            boot_id,
            request,
        });
        cancelled
    }

    pub(crate) fn send_next(
        &mut self,
        endpoint_id: &ClientEndpointId,
        endpoints: &mut EndpointRegistry,
        now: Instant,
    ) -> EndpointCommandCancellation {
        let lane = self.lanes.entry(endpoint_id.clone()).or_default();
        let mut cancelled = EndpointCommandCancellation::default();
        if lane.in_flight.is_some() {
            return cancelled;
        }
        while let Some(queued) = lane.queued.pop_front() {
            let ClientShellEndpointRequest { id, command } = *queued.request;
            if !endpoints.accepts(endpoint_id, queued.generation) {
                cancelled.unsent.push(id);
                continue;
            }
            let request_id = id;
            let message = ClientMessage::ClientShellEndpointRequest {
                boot_id: queued.boot_id.clone(),
                request_id: request_id.clone(),
                command,
            };
            if endpoints.send_to(endpoint_id, &message) != EndpointSendOutcome::Sent {
                cancelled.possibly_sent.push(request_id);
                continue;
            }
            lane.in_flight = Some(InFlightCommand {
                key: RequestKey {
                    generation: queued.generation,
                    boot_id: queued.boot_id,
                    request_id,
                },
                sent_at: now,
            });
            break;
        }
        cancelled
    }

    /// Retire the complete lane when an endpoint stops being shown. The in-flight request is
    /// released so late responses are ignored; every queued request is cancelled before
    /// it can run while another endpoint is shown. Other endpoint lanes are deliberately
    /// untouched.
    pub(crate) fn retire_lane(
        &mut self,
        endpoint_id: &ClientEndpointId,
    ) -> EndpointCommandCancellation {
        let Some(lane) = self.lanes.get_mut(endpoint_id) else {
            return EndpointCommandCancellation::default();
        };
        let mut cancelled = EndpointCommandCancellation::default();
        if let Some(command) = lane.in_flight.take() {
            cancelled.possibly_sent.push(command.key.request_id);
        }
        cancelled
            .unsent
            .extend(lane.queued.drain(..).map(|command| command.request.id));
        cancelled
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.lanes
            .values()
            .filter_map(|lane| lane.in_flight.as_ref())
            .filter_map(|command| command.sent_at.checked_add(ENDPOINT_COMMAND_TIMEOUT))
            .min()
    }

    pub(crate) fn expire(&mut self, now: Instant) -> Vec<EndpointCommandResult> {
        self.lanes
            .iter_mut()
            .filter_map(|(endpoint_id, lane)| {
                let command = lane.in_flight.as_ref()?;
                if now.saturating_duration_since(command.sent_at) < ENDPOINT_COMMAND_TIMEOUT {
                    return None;
                }
                let command = lane.in_flight.take()?;
                Some(EndpointCommandResult {
                    endpoint_id: endpoint_id.clone(),
                    generation: command.key.generation,
                    boot_id: command.key.boot_id,
                    request_id: command.key.request_id,
                    result: Err(ClientShellEndpointError::Timeout),
                })
            })
            .collect()
    }

    /// Completes only the in-flight command this response answers. Retired,
    /// expired and unknown requests are ignored without retaining their identities.
    pub(crate) fn receive_response(
        &mut self,
        endpoint_id: &ClientEndpointId,
        response_generation: ConnectionGeneration,
        response_boot_id: &BootId,
        response_request_id: &RequestId,
        result: Result<EndpointReply, EndpointError>,
    ) -> Option<EndpointCommandResult> {
        let lane = self.lanes.get_mut(endpoint_id)?;
        let key = RequestKey {
            generation: response_generation,
            boot_id: response_boot_id.clone(),
            request_id: response_request_id.clone(),
        };
        if lane.in_flight.as_ref()?.key != key {
            return None;
        }
        let in_flight = lane.in_flight.take()?;
        Some(EndpointCommandResult {
            endpoint_id: endpoint_id.clone(),
            generation: in_flight.key.generation,
            boot_id: in_flight.key.boot_id,
            request_id: in_flight.key.request_id,
            result: result.map_err(ClientShellEndpointError::from),
        })
    }

    /// Disconnecting an endpoint also cancels its shell-pending requests. Connection generation
    /// rejection handles any late wire response after the lane itself is removed.
    pub(crate) fn disconnect(
        &mut self,
        endpoint_id: &ClientEndpointId,
    ) -> EndpointCommandCancellation {
        let Some(lane) = self.lanes.remove(endpoint_id) else {
            return EndpointCommandCancellation::default();
        };
        let mut cancelled = EndpointCommandCancellation {
            unsent: lane
                .queued
                .into_iter()
                .map(|command| command.request.id)
                .collect(),
            possibly_sent: Vec::new(),
        };
        if let Some(command) = lane.in_flight {
            cancelled.possibly_sent.push(command.key.request_id);
        }
        cancelled
    }
}

/// Whether two ratio commands set the same split: the same workspace, and the same path at
/// the same layout epoch (a topology change can put another split at a path).
fn same_split(a: &LayoutSetSplitRatioParams, b: &LayoutSetSplitRatioParams) -> bool {
    a.workspace_id == b.workspace_id && a.path == b.path && a.epoch == b.epoch
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::test_generation as generation;
    use std::time::Duration;

    fn endpoint() -> ClientEndpointId {
        ClientEndpointId::Local
    }

    fn boot_a() -> BootId {
        shepr_test_fixtures::fixed_boot_id(1)
    }

    fn boot_b() -> BootId {
        shepr_test_fixtures::fixed_boot_id(2)
    }

    fn request_a() -> RequestId {
        static ID: std::sync::OnceLock<RequestId> = std::sync::OnceLock::new();
        ID.get_or_init(RequestId::allocate).clone()
    }

    fn request_b() -> RequestId {
        static ID: std::sync::OnceLock<RequestId> = std::sync::OnceLock::new();
        ID.get_or_init(RequestId::allocate).clone()
    }

    fn commands_with_in_flight() -> EndpointCommands {
        EndpointCommands {
            lanes: HashMap::from([(
                endpoint(),
                EndpointCommandLane {
                    in_flight: Some(InFlightCommand {
                        key: RequestKey {
                            generation: generation(1),
                            boot_id: boot_a(),
                            request_id: request_a(),
                        },
                        sent_at: Instant::now(),
                    }),
                    ..EndpointCommandLane::default()
                },
            )]),
        }
    }

    fn has_in_flight(commands: &EndpointCommands) -> bool {
        commands
            .lanes
            .get(&endpoint())
            .is_some_and(|lane| lane.in_flight.is_some())
    }

    fn queued(request_id: RequestId, position: u64, boot_id: BootId) -> QueuedCommand {
        QueuedCommand {
            generation: generation(position),
            boot_id,
            request: Box::new(ClientShellEndpointRequest {
                id: request_id,
                command: shepr_protocol::command::EndpointCommand::PaneClear(
                    shepr_protocol::command::PaneTarget {
                        pane_id: shepr_test_fixtures::id("w1:p1"),
                    },
                ),
            }),
        }
    }

    fn ratio_request(
        path: Vec<shepr_core::layout::SplitBranch>,
        ratio: f32,
    ) -> Box<ClientShellEndpointRequest> {
        Box::new(ClientShellEndpointRequest {
            id: RequestId::allocate(),
            command: EndpointCommand::LayoutSetSplitRatio(LayoutSetSplitRatioParams {
                workspace_id: shepr_test_fixtures::id("w1"),
                path,
                epoch: shepr_core::layout::LayoutEpoch::default(),
                ratio: shepr_core::layout::SplitRatio::clamped(ratio),
            }),
        })
    }

    fn queued_ids(commands: &EndpointCommands) -> Vec<RequestId> {
        commands
            .lanes
            .get(&endpoint())
            .map_or_else(Vec::new, |lane| {
                lane.queued
                    .iter()
                    .map(|command| command.request.id.clone())
                    .collect()
            })
    }

    #[test]
    fn a_queued_split_ratio_is_replaced_by_a_newer_one_for_the_same_split() {
        let mut commands = commands_with_in_flight();
        let first = ratio_request(Vec::new(), 0.4);
        let first_id = first.id.clone();
        assert_eq!(
            commands.enqueue(endpoint(), generation(1), boot_a(), first),
            EndpointCommandCancellation::default()
        );
        let other_split = ratio_request(vec![shepr_core::layout::SplitBranch::Second], 0.3);
        let other_id = other_split.id.clone();
        assert_eq!(
            commands.enqueue(endpoint(), generation(1), boot_a(), other_split),
            EndpointCommandCancellation::default()
        );
        let newer = ratio_request(Vec::new(), 0.6);
        let newer_id = newer.id.clone();

        assert_eq!(
            commands.enqueue(endpoint(), generation(1), boot_a(), newer),
            EndpointCommandCancellation {
                unsent: vec![first_id],
                possibly_sent: Vec::new(),
            }
        );
        assert_eq!(queued_ids(&commands), vec![other_id, newer_id]);
        // The in-flight command is not part of the queue and stays in flight.
        assert!(has_in_flight(&commands));
    }

    #[test]
    fn a_split_ratio_queued_for_another_connection_or_boot_is_not_replaced() {
        let mut commands = commands_with_in_flight();
        let older_generation = ratio_request(Vec::new(), 0.4);
        let older_generation_id = older_generation.id.clone();
        let older_boot = ratio_request(Vec::new(), 0.45);
        let older_boot_id = older_boot.id.clone();
        let current = ratio_request(Vec::new(), 0.6);
        let current_id = current.id.clone();
        for (at, boot, request) in [
            (generation(1), boot_a(), older_generation),
            (generation(2), boot_b(), older_boot),
            (generation(2), boot_a(), current),
        ] {
            assert_eq!(
                commands.enqueue(endpoint(), at, boot, request),
                EndpointCommandCancellation::default()
            );
        }
        assert_eq!(
            queued_ids(&commands),
            vec![older_generation_id, older_boot_id, current_id]
        );
    }

    #[test]
    fn a_response_matches_only_the_in_flight_request() {
        let mut commands = commands_with_in_flight();
        for (at, boot, id) in [
            (generation(2), boot_a(), request_a()),
            (generation(1), boot_b(), request_a()),
            (generation(1), boot_a(), RequestId::allocate()),
        ] {
            assert!(
                commands
                    .receive_response(&endpoint(), at, &boot, &id, Ok(EndpointReply::Done))
                    .is_none()
            );
        }
        assert!(
            commands
                .receive_response(
                    &endpoint(),
                    generation(1),
                    &boot_a(),
                    &request_a(),
                    Ok(EndpointReply::Done)
                )
                .is_some()
        );
    }

    #[test]
    fn response_completion_is_correlated_and_clears_the_lane() {
        let mut commands = commands_with_in_flight();
        let completed = commands
            .receive_response(
                &endpoint(),
                generation(1),
                &boot_a(),
                &request_a(),
                Ok(EndpointReply::PaneSelection {
                    pane_id: shepr_test_fixtures::id("w1:p1"),
                    text: "selected".into(),
                }),
            )
            .expect("test precondition");

        assert_eq!(completed.endpoint_id, endpoint());
        assert_eq!(completed.generation, generation(1));
        assert_eq!(completed.boot_id, boot_a());
        assert_eq!(completed.request_id, request_a());
        assert!(matches!(
            completed.result,
            Ok(EndpointReply::PaneSelection { text, .. }) if text == "selected"
        ));
        assert!(!has_in_flight(&commands));
    }

    #[test]
    fn server_errors_are_wrapped_typed() {
        let mut commands = commands_with_in_flight();
        let completed = commands
            .receive_response(
                &endpoint(),
                generation(1),
                &boot_a(),
                &request_a(),
                Err(EndpointError::LimitExceeded(
                    shepr_protocol::LimitExceeded::new(
                        shepr_protocol::Limit::new(
                            shepr_protocol::LimitKind::EndpointResponseBytes,
                            8,
                        ),
                        9,
                    ),
                )),
            )
            .expect("test precondition");
        assert!(matches!(
            completed.result,
            Err(ClientShellEndpointError::Server(
                EndpointError::LimitExceeded(exceeded)
            )) if exceeded == shepr_protocol::LimitExceeded::new(
                shepr_protocol::Limit::new(shepr_protocol::LimitKind::EndpointResponseBytes, 8),
                9,
            )
        ));
    }

    #[test]
    fn in_flight_endpoint_command_expires_and_releases_the_lane() {
        let mut commands = commands_with_in_flight();
        let start = Instant::now();
        commands
            .lanes
            .get_mut(&endpoint())
            .expect("test command lane")
            .in_flight
            .as_mut()
            .expect("test in-flight command")
            .sent_at = start;
        let deadline = start + ENDPOINT_COMMAND_TIMEOUT;
        assert_eq!(commands.next_deadline(), Some(deadline));
        assert!(
            commands
                .expire(start + ENDPOINT_COMMAND_TIMEOUT - Duration::from_nanos(1))
                .is_empty()
        );
        let expired = commands
            .expire(start + ENDPOINT_COMMAND_TIMEOUT)
            .pop()
            .expect("expired endpoint command");

        assert_eq!(expired.endpoint_id, endpoint());
        assert_eq!(expired.boot_id, boot_a());
        assert_eq!(expired.request_id, request_a());
        assert!(matches!(
            expired.result,
            Err(ClientShellEndpointError::Timeout)
        ));
        assert!(!has_in_flight(&commands));
        assert_eq!(commands.next_deadline(), None);
        assert!(commands.expire(start + ENDPOINT_COMMAND_TIMEOUT).is_empty());
        assert!(
            commands
                .receive_response(
                    &endpoint(),
                    generation(1),
                    &boot_a(),
                    &request_a(),
                    Ok(EndpointReply::Done)
                )
                .is_none()
        );
        assert!(!has_in_flight(&commands));
    }

    #[test]
    fn endpoint_lanes_complete_independently() {
        let remote = ClientEndpointId::Ssh(
            crate::endpoint::MachineLabel::parse("build").expect("test precondition"),
        );
        let mut commands = commands_with_in_flight();
        commands.lanes.insert(
            remote.clone(),
            EndpointCommandLane {
                in_flight: Some(InFlightCommand {
                    key: RequestKey {
                        generation: generation(2),
                        boot_id: boot_b(),
                        request_id: request_b(),
                    },
                    sent_at: Instant::now(),
                }),
                ..EndpointCommandLane::default()
            },
        );

        let completed = commands
            .receive_response(
                &remote,
                generation(2),
                &boot_b(),
                &request_b(),
                Ok(EndpointReply::Done),
            )
            .expect("remote response");

        assert_eq!(completed.endpoint_id, remote);
        assert!(has_in_flight(&commands));
        assert!(
            commands
                .lanes
                .get(&completed.endpoint_id)
                .is_some_and(|lane| lane.in_flight.is_none())
        );
    }

    #[test]
    fn retiring_complete_source_lane_cancels_queued_ids_and_keeps_other_lanes() {
        let remote = ClientEndpointId::Ssh(
            crate::endpoint::MachineLabel::parse("build").expect("test precondition"),
        );
        let mut commands = commands_with_in_flight();
        let source = RequestId::allocate();
        commands
            .lanes
            .get_mut(&endpoint())
            .expect("test precondition")
            .queued
            .push_back(queued(source.clone(), 1, boot_a()));
        commands.lanes.insert(
            remote.clone(),
            EndpointCommandLane {
                queued: VecDeque::from([queued(request_b(), 2, boot_b())]),
                ..EndpointCommandLane::default()
            },
        );

        assert_eq!(
            commands.retire_lane(&endpoint()),
            EndpointCommandCancellation {
                unsent: vec![source],
                possibly_sent: vec![request_a()],
            }
        );
        assert!(!has_in_flight(&commands));
        assert!(
            commands
                .lanes
                .get(&endpoint())
                .is_some_and(|lane| lane.queued.is_empty())
        );
        assert!(
            commands
                .lanes
                .get(&remote)
                .is_some_and(|lane| !lane.queued.is_empty())
        );

        assert!(
            commands
                .receive_response(
                    &endpoint(),
                    generation(1),
                    &boot_a(),
                    &request_a(),
                    Ok(EndpointReply::Done)
                )
                .is_none()
        );
    }

    #[test]
    fn disconnect_returns_every_request_that_must_be_discarded() {
        let mut commands = commands_with_in_flight();
        let queued_a = RequestId::allocate();
        commands
            .lanes
            .get_mut(&endpoint())
            .expect("test precondition")
            .queued
            .push_back(queued(queued_a.clone(), 1, boot_a()));
        assert_eq!(
            commands.disconnect(&endpoint()),
            EndpointCommandCancellation {
                unsent: vec![queued_a],
                possibly_sent: vec![request_a()],
            }
        );
        assert!(!commands.lanes.contains_key(&endpoint()));
    }

    #[test]
    fn stale_or_unknown_responses_do_not_damage_the_live_lane() {
        let mut commands = commands_with_in_flight();
        let unknown = ClientEndpointId::Ssh(
            crate::endpoint::MachineLabel::parse("build").expect("test precondition"),
        );

        assert!(
            commands
                .receive_response(
                    &endpoint(),
                    generation(2),
                    &boot_a(),
                    &request_a(),
                    Ok(EndpointReply::Done)
                )
                .is_none()
        );
        assert!(
            commands
                .receive_response(
                    &endpoint(),
                    generation(1),
                    &boot_b(),
                    &request_a(),
                    Ok(EndpointReply::Done)
                )
                .is_none()
        );
        assert!(
            commands
                .receive_response(
                    &unknown,
                    generation(1),
                    &boot_a(),
                    &request_a(),
                    Ok(EndpointReply::Done)
                )
                .is_none()
        );
        assert!(has_in_flight(&commands));
    }

    #[test]
    fn stale_queued_request_is_cancelled_without_blocking_the_current_generation() {
        use crate::shell::tests::{pending_request, request_id};
        use crate::shell::{ClientShellAction, DropReason, LocationTarget};

        let (mut state, actions) = pending_request();
        let stale_id = request_id(&actions).to_owned();
        let current =
            state.focus_endpoint_target(LocationTarget::Workspace(shepr_test_fixtures::id("w1")));
        let current_id = request_id(&current).to_owned();
        let mut commands = EndpointCommands::default();
        for (generation, actions) in [(generation(1), actions), (generation(2), current)] {
            for action in actions {
                let ClientShellAction::Endpoint {
                    endpoint_id,
                    boot_id,
                    request,
                } = action
                else {
                    panic!("expected endpoint request");
                };
                commands.enqueue(endpoint_id, generation, boot_id, request);
            }
        }
        let mut endpoints = EndpointRegistry::new(
            crate::tests::endpoints::RecordingTransport::default(),
            generation(2),
        );
        let cancelled = commands.send_next(&endpoint(), &mut endpoints, Instant::now());
        assert_eq!(cancelled.unsent, vec![stale_id.clone()]);
        assert!(cancelled.possibly_sent.is_empty());
        state.drop_request(&stale_id, DropReason::Unsent);
        assert_eq!(state.visible_notice_title(), None);
        assert!(
            commands
                .receive_response(
                    &endpoint(),
                    generation(1),
                    &crate::tests::test_boot_id("boot-1"),
                    &stale_id,
                    Ok(EndpointReply::Done)
                )
                .is_none()
        );
        assert!(
            commands
                .receive_response(
                    &endpoint(),
                    generation(2),
                    &crate::tests::test_boot_id("boot-1"),
                    &current_id,
                    Ok(EndpointReply::Done)
                )
                .is_some()
        );
        assert!(state.has_request(&current_id));
    }
}
