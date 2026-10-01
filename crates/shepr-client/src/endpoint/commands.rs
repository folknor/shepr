use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use shepr_protocol::command::{EndpointError, EndpointReply};
use shepr_protocol::{BootId, ClientMessage, ConnectionGeneration, RequestId};

use super::{ClientEndpointId, EndpointRegistry, EndpointSendOutcome};
use crate::limits::{ENDPOINT_COMMAND_TIMEOUT, MAX_RETIRED_REQUESTS_PER_ENDPOINT};
use crate::shell::{ClientShellEndpointError, ClientShellEndpointRequest};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandResponseKind {
    Active,
    Retired,
    Untracked,
}

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
    /// The connection generation the command was sent on. A timed-out
    /// command is only reported while that connection is still current.
    pub(crate) generation: u64,
    pub(crate) boot_id: BootId,
    pub(crate) request_id: RequestId,
    pub(crate) result: Result<EndpointReply, ClientShellEndpointError>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct EndpointCommandCancellation {
    /// Commands rejected before a transport send was attempted.
    pub(crate) unsent: Vec<String>,
    /// Commands that were in flight or whose transport send failed. The server
    /// may have received them, so their cancellation is reported as uncertain.
    pub(crate) possibly_sent: Vec<String>,
}

#[derive(Default)]
struct EndpointCommandLane {
    queued: VecDeque<QueuedCommand>,
    in_flight: Option<InFlightCommand>,
    retired: VecDeque<RequestKey>,
}

impl EndpointCommandLane {
    fn retire(&mut self, request: RequestKey) {
        if self.retired.contains(&request) {
            return;
        }
        if self.retired.len() == MAX_RETIRED_REQUESTS_PER_ENDPOINT {
            self.retired.pop_front();
        }
        self.retired.push_back(request);
    }

    /// Whether `request` was retired, dropping its tombstone: its one response
    /// has now arrived.
    fn consume_retired(&mut self, request: &RequestKey) -> bool {
        let Some(index) = self.retired.iter().position(|retired| retired == request) else {
            return false;
        };
        self.retired.remove(index);
        true
    }
}

#[derive(Default)]
pub(crate) struct EndpointCommands {
    lanes: HashMap<ClientEndpointId, EndpointCommandLane>,
}

impl EndpointCommands {
    pub(crate) fn response_kind(
        &self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        boot_id: &str,
        request_id: &str,
    ) -> CommandResponseKind {
        let Some(lane) = self.lanes.get(endpoint_id) else {
            return CommandResponseKind::Untracked;
        };
        if lane.in_flight.as_ref().is_some_and(|command| {
            command.key.generation == generation
                && command.key.boot_id == boot_id
                && command.key.request_id == request_id
        }) {
            return CommandResponseKind::Active;
        }
        if lane.retired.iter().any(|retired| {
            retired.generation == generation
                && retired.boot_id == boot_id
                && retired.request_id == request_id
        }) {
            return CommandResponseKind::Retired;
        }
        CommandResponseKind::Untracked
    }

    pub(crate) fn enqueue(
        &mut self,
        endpoint_id: ClientEndpointId,
        generation: u64,
        boot_id: BootId,
        request: Box<ClientShellEndpointRequest>,
    ) {
        self.lanes
            .entry(endpoint_id)
            .or_default()
            .queued
            .push_back(QueuedCommand {
                generation: generation.into(),
                boot_id,
                request,
            });
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
            if !endpoints.accepts(endpoint_id, queued.generation.get()) {
                cancelled.unsent.push(id);
                continue;
            }
            let request_id = RequestId::from(id);
            let message = ClientMessage::ClientShellEndpointRequest {
                boot_id: queued.boot_id.clone(),
                request_id: request_id.clone(),
                command,
            };
            if endpoints.send_to(endpoint_id, &message) != EndpointSendOutcome::Sent {
                cancelled.possibly_sent.push(request_id.to_string());
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
    /// tombstoned for a late endpoint-local response; every queued request is cancelled before
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
            cancelled
                .possibly_sent
                .push(command.key.request_id.to_string());
            lane.retire(command.key);
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
                lane.retire(command.key.clone());
                Some(EndpointCommandResult {
                    endpoint_id: endpoint_id.clone(),
                    generation: command.key.generation.get(),
                    boot_id: command.key.boot_id,
                    request_id: command.key.request_id,
                    result: Err(ClientShellEndpointError::Timeout),
                })
            })
            .collect()
    }

    /// Completes the in-flight command a response answers. A response to a
    /// retired command only clears its tombstone, and one that matches
    /// nothing in flight is ignored.
    pub(crate) fn receive_response(
        &mut self,
        endpoint_id: &ClientEndpointId,
        response_generation: u64,
        response_boot_id: &BootId,
        response_request_id: &RequestId,
        result: Result<EndpointReply, EndpointError>,
    ) -> Option<EndpointCommandResult> {
        let lane = self.lanes.get_mut(endpoint_id)?;
        let key = RequestKey {
            generation: response_generation.into(),
            boot_id: response_boot_id.clone(),
            request_id: response_request_id.clone(),
        };
        if lane.consume_retired(&key) {
            return None;
        }
        if lane.in_flight.as_ref()?.key != key {
            return None;
        }
        let in_flight = lane.in_flight.take()?;
        Some(EndpointCommandResult {
            endpoint_id: endpoint_id.clone(),
            generation: in_flight.key.generation.get(),
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
            cancelled
                .possibly_sent
                .push(command.key.request_id.to_string());
        }
        cancelled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        "request-a".into()
    }

    fn commands_with_in_flight() -> EndpointCommands {
        EndpointCommands {
            lanes: HashMap::from([(
                endpoint(),
                EndpointCommandLane {
                    in_flight: Some(InFlightCommand {
                        key: RequestKey {
                            generation: ConnectionGeneration::new(1),
                            boot_id: boot_a(),
                            request_id: "request-a".into(),
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

    fn queued(request_id: &str, generation: u64, boot_id: BootId) -> QueuedCommand {
        QueuedCommand {
            generation: ConnectionGeneration::new(generation),
            boot_id,
            request: Box::new(ClientShellEndpointRequest {
                id: request_id.into(),
                command: shepr_protocol::command::EndpointCommand::PaneClear(
                    shepr_protocol::command::PaneTarget {
                        pane_id: shepr_test_fixtures::id("w1:p1"),
                    },
                ),
            }),
        }
    }

    #[test]
    fn response_kind_uses_tracked_identity_instead_of_id_text() {
        let mut commands = commands_with_in_flight();
        assert_eq!(
            commands.response_kind(&endpoint(), 1, &boot_a(), "request-a"),
            CommandResponseKind::Active
        );
        assert_eq!(
            commands.response_kind(&endpoint(), 1, &boot_a(), "client-shell-surface:1:on"),
            CommandResponseKind::Untracked
        );
        commands.retire_lane(&endpoint());
        assert_eq!(
            commands.response_kind(&endpoint(), 1, &boot_a(), "request-a"),
            CommandResponseKind::Retired
        );
    }

    #[test]
    fn response_completion_is_correlated_and_clears_the_lane() {
        let mut commands = commands_with_in_flight();
        let completed = commands
            .receive_response(
                &endpoint(),
                1,
                &boot_a(),
                &request_a(),
                Ok(EndpointReply::PaneSelection {
                    pane_id: shepr_test_fixtures::id("w1:p1"),
                    text: "selected".into(),
                }),
            )
            .expect("test precondition");

        assert_eq!(completed.endpoint_id, endpoint());
        assert_eq!(completed.generation, 1);
        assert_eq!(completed.boot_id, boot_a());
        assert_eq!(completed.request_id, "request-a");
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
                1,
                &boot_a(),
                &request_a(),
                Err(EndpointError::ResponseTooLarge { size: 9, limit: 8 }),
            )
            .expect("test precondition");
        assert!(matches!(
            completed.result,
            Err(ClientShellEndpointError::Server(
                EndpointError::ResponseTooLarge { size: 9, limit: 8 }
            ))
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
        assert_eq!(expired.request_id, "request-a");
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
                    1,
                    &boot_a(),
                    &request_a(),
                    Ok(EndpointReply::Done)
                )
                .is_none()
        );
        assert!(!has_in_flight(&commands));
        assert_eq!(
            commands.response_kind(&endpoint(), 1, &boot_a(), "request-a"),
            CommandResponseKind::Untracked,
            "the late response consumed its tombstone"
        );
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
                        generation: ConnectionGeneration::new(2),
                        boot_id: boot_b(),
                        request_id: "request-b".into(),
                    },
                    sent_at: Instant::now(),
                }),
                ..EndpointCommandLane::default()
            },
        );

        let completed = commands
            .receive_response(
                &remote,
                2,
                &boot_b(),
                &"request-b".into(),
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
        commands
            .lanes
            .get_mut(&endpoint())
            .expect("test precondition")
            .queued
            .push_back(queued("queued-source", 1, boot_a()));
        commands.lanes.insert(
            remote.clone(),
            EndpointCommandLane {
                queued: VecDeque::from([queued("request-b", 2, boot_b())]),
                ..EndpointCommandLane::default()
            },
        );

        assert_eq!(
            commands.retire_lane(&endpoint()),
            EndpointCommandCancellation {
                unsent: vec!["queued-source".into()],
                possibly_sent: vec!["request-a".into()],
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
                    1,
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
        commands
            .lanes
            .get_mut(&endpoint())
            .expect("test precondition")
            .queued
            .push_back(queued("queued-a", 1, boot_a()));
        assert_eq!(
            commands.disconnect(&endpoint()),
            EndpointCommandCancellation {
                unsent: vec!["queued-a".into()],
                possibly_sent: vec!["request-a".into()],
            }
        );
        assert!(!commands.lanes.contains_key(&endpoint()));
    }

    #[test]
    fn retired_request_tombstones_are_bounded() {
        let mut lane = EndpointCommandLane::default();
        for serial in 0..MAX_RETIRED_REQUESTS_PER_ENDPOINT + 10 {
            lane.retire(RequestKey {
                generation: ConnectionGeneration::new(1),
                boot_id: boot_a(),
                request_id: format!("request-{serial}").into(),
            });
        }
        assert_eq!(lane.retired.len(), MAX_RETIRED_REQUESTS_PER_ENDPOINT);
        assert!(!lane.retired.contains(&RequestKey {
            generation: ConnectionGeneration::new(1),
            boot_id: boot_a(),
            request_id: "request-0".into(),
        }));
        assert!(lane.retired.contains(&RequestKey {
            generation: ConnectionGeneration::new(1),
            boot_id: boot_a(),
            request_id: format!("request-{}", MAX_RETIRED_REQUESTS_PER_ENDPOINT + 9).into(),
        }));
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
                    2,
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
                    1,
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
                    1,
                    &boot_a(),
                    &request_a(),
                    Ok(EndpointReply::Done)
                )
                .is_none()
        );
        assert!(has_in_flight(&commands));
    }
}
