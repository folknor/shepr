use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use shepr_api::client::ApiClientError;
use shepr_api::schema::{Request, ResponseResult};
use shepr_protocol::{BootId, ClientMessage, ConnectionGeneration, RequestId};

use super::{ClientEndpointId, EndpointRegistry, EndpointSendOutcome};
use crate::limits::{
    ENDPOINT_COMMAND_TIMEOUT, MAX_ENDPOINT_RESPONSE_BYTES, MAX_RETIRED_REQUESTS_PER_ENDPOINT,
};
use crate::shell::ClientShellEndpointError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EndpointFailureCode {
    Timeout,
    ResponseTooLarge,
    Cancelled,
    Remote(String),
}

impl EndpointFailureCode {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Timeout => "endpoint_timeout",
            Self::ResponseTooLarge => "endpoint_response_too_large",
            Self::Cancelled => "endpoint_cancelled",
            Self::Remote(code) => code,
        }
    }
}

impl From<String> for EndpointFailureCode {
    fn from(code: String) -> Self {
        match code.as_str() {
            "endpoint_timeout" => Self::Timeout,
            "endpoint_response_too_large" => Self::ResponseTooLarge,
            "endpoint_cancelled" => Self::Cancelled,
            _ => Self::Remote(code),
        }
    }
}

impl From<&str> for EndpointFailureCode {
    fn from(code: &str) -> Self {
        code.to_owned().into()
    }
}

impl PartialEq<&str> for EndpointFailureCode {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandResponseKind {
    Active,
    Retired,
    Untracked,
}

struct QueuedCommand {
    generation: ConnectionGeneration,
    boot_id: BootId,
    request: Box<Request>,
}

struct InFlightCommand {
    key: RequestKey,
    response: Vec<u8>,
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
    pub(crate) generation: u64,
    pub(crate) boot_id: BootId,
    pub(crate) request_id: RequestId,
    pub(crate) result: Result<ResponseResult, ClientShellEndpointError>,
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

    fn consume_retired(&mut self, request: &RequestKey, final_chunk: bool) -> bool {
        let Some(index) = self.retired.iter().position(|retired| retired == request) else {
            return false;
        };
        if final_chunk {
            self.retired.remove(index);
        }
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
        request: Box<Request>,
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
    ) -> Vec<String> {
        let lane = self.lanes.entry(endpoint_id.clone()).or_default();
        let mut cancelled = Vec::new();
        if lane.in_flight.is_some() {
            return cancelled;
        }
        while let Some(queued) = lane.queued.pop_front() {
            let request_id = queued.request.id.clone();
            if !endpoints.accepts(endpoint_id, queued.generation.get()) {
                cancelled.push(request_id);
                continue;
            }
            let request = match serde_json::to_string(&queued.request) {
                Ok(request) => request,
                Err(error) => {
                    tracing::warn!(%error, %request_id, "could not encode endpoint request");
                    cancelled.push(request_id);
                    continue;
                }
            };
            let message = ClientMessage::ClientShellEndpointRequest {
                boot_id: queued.boot_id.clone(),
                request,
            };
            if endpoints.send_to(endpoint_id, &message) != EndpointSendOutcome::Sent {
                cancelled.push(request_id);
                continue;
            }
            lane.in_flight = Some(InFlightCommand {
                key: RequestKey {
                    generation: queued.generation,
                    boot_id: queued.boot_id,
                    request_id: request_id.into(),
                },
                response: Vec::new(),
                sent_at: now,
            });
            break;
        }
        cancelled
    }

    pub(crate) fn accepts_response(
        &self,
        endpoint_id: &ClientEndpointId,
        response_generation: u64,
        response_boot_id: &str,
        response_request_id: &str,
    ) -> bool {
        self.lanes
            .get(endpoint_id)
            .and_then(|lane| lane.in_flight.as_ref())
            .is_some_and(|command| {
                command.key.generation == response_generation
                    && command.key.boot_id == response_boot_id
                    && command.key.request_id == response_request_id
            })
    }

    /// Retire the complete source lane at source-off. The in-flight request is tombstoned for a
    /// late endpoint-local response; every queued request is cancelled before it can run in a
    /// later presentation epoch. Other endpoint lanes are deliberately untouched.
    pub(crate) fn retire_lane(&mut self, endpoint_id: &ClientEndpointId) -> Vec<String> {
        let Some(lane) = self.lanes.get_mut(endpoint_id) else {
            return Vec::new();
        };
        let mut request_ids = Vec::new();
        if let Some(command) = lane.in_flight.take() {
            request_ids.push(command.key.request_id.to_string());
            lane.retire(command.key);
        }
        request_ids.extend(lane.queued.drain(..).map(|command| command.request.id));
        request_ids
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
                    result: Err(ClientShellEndpointError {
                        code: Some(EndpointFailureCode::Timeout),
                        message: "this server did not respond to the action".into(),
                    }),
                })
            })
            .collect()
    }

    pub(crate) fn receive_chunk(
        &mut self,
        endpoint_id: &ClientEndpointId,
        response_generation: u64,
        response_boot_id: &BootId,
        response_request_id: &RequestId,
        final_chunk: bool,
        data: Vec<u8>,
    ) -> Option<EndpointCommandResult> {
        let lane = self.lanes.get_mut(endpoint_id)?;
        let retired = RequestKey {
            generation: response_generation.into(),
            boot_id: response_boot_id.clone(),
            request_id: response_request_id.clone(),
        };
        if lane.consume_retired(&retired, final_chunk) {
            return None;
        }
        let in_flight = lane.in_flight.as_mut()?;
        if response_generation != in_flight.key.generation
            || *response_boot_id != in_flight.key.boot_id
            || *response_request_id != in_flight.key.request_id
        {
            return None;
        }
        // The command timeout alone would let an endpoint grow this buffer for a full minute.
        if in_flight.response.len().saturating_add(data.len()) > MAX_ENDPOINT_RESPONSE_BYTES {
            let in_flight = lane.in_flight.take()?;
            if !final_chunk {
                // Swallow the rest of this response instead of misreading it as unsolicited.
                lane.retire(in_flight.key.clone());
            }
            return Some(EndpointCommandResult {
                endpoint_id: endpoint_id.clone(),
                generation: in_flight.key.generation.get(),
                boot_id: in_flight.key.boot_id,
                request_id: in_flight.key.request_id,
                result: Err(ClientShellEndpointError {
                    code: Some(EndpointFailureCode::ResponseTooLarge),
                    message: format!(
                        "this server's response exceeded {} MiB",
                        MAX_ENDPOINT_RESPONSE_BYTES / (1024 * 1024)
                    ),
                }),
            });
        }
        in_flight.response.extend(data);
        if !final_chunk {
            return None;
        }

        let in_flight = lane.in_flight.take()?;
        let result = parse_response(&in_flight.key.request_id, &in_flight.response);
        Some(EndpointCommandResult {
            endpoint_id: endpoint_id.clone(),
            generation: in_flight.key.generation.get(),
            boot_id: in_flight.key.boot_id,
            request_id: in_flight.key.request_id,
            result,
        })
    }

    /// Disconnecting an endpoint also cancels its shell-pending requests. Connection generation
    /// rejection handles any late wire response after the lane itself is removed.
    pub(crate) fn disconnect(&mut self, endpoint_id: &ClientEndpointId) -> Vec<String> {
        let Some(lane) = self.lanes.remove(endpoint_id) else {
            return Vec::new();
        };
        let mut request_ids = lane
            .queued
            .into_iter()
            .map(|command| command.request.id)
            .collect::<Vec<_>>();
        if let Some(command) = lane.in_flight {
            request_ids.push(command.key.request_id.to_string());
        }
        request_ids
    }
}

pub(crate) fn parse_response(
    expected_id: &str,
    response: &[u8],
) -> Result<ResponseResult, ClientShellEndpointError> {
    let value = serde_json::from_slice(response).map_err(|error| ClientShellEndpointError {
        code: None,
        message: format!("invalid endpoint response: {error}"),
    })?;
    match shepr_api::client::parse_response_value(value) {
        Ok(response) if response.id == expected_id => Ok(response.result),
        Ok(response) => Err(ClientShellEndpointError {
            code: None,
            message: format!(
                "endpoint response id {:?} did not match {expected_id:?}",
                response.id
            ),
        }),
        Err(ApiClientError::ErrorResponse(response)) if response.id == expected_id => {
            Err(ClientShellEndpointError {
                code: Some(response.error.code.into()),
                message: response.error.message,
            })
        }
        Err(ApiClientError::ErrorResponse(response)) => Err(ClientShellEndpointError {
            code: None,
            message: format!(
                "endpoint error id {:?} did not match {expected_id:?}",
                response.id
            ),
        }),
        Err(error) => Err(ClientShellEndpointError {
            code: None,
            message: error.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_api::schema::{ResponseResult, SuccessResponse};
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
                        response: Vec::new(),
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
    fn chunked_response_completion_is_correlated_and_clears_the_lane() {
        let mut commands = commands_with_in_flight();
        let response = serde_json::to_string(&SuccessResponse {
            id: "request-a".into(),
            result: ResponseResult::Ok {},
        })
        .expect("test precondition");
        let split = response.len() / 2;

        assert!(
            commands
                .receive_chunk(
                    &endpoint(),
                    1,
                    &boot_a(),
                    &request_a(),
                    false,
                    response.as_bytes()[..split].to_vec(),
                )
                .is_none()
        );
        let completed = commands
            .receive_chunk(
                &endpoint(),
                1,
                &boot_a(),
                &request_a(),
                true,
                response.as_bytes()[split..].to_vec(),
            )
            .expect("test precondition");

        assert_eq!(completed.endpoint_id, endpoint());
        assert_eq!(completed.generation, 1);
        assert_eq!(completed.boot_id, boot_a());
        assert_eq!(completed.request_id, "request-a");
        assert!(matches!(completed.result, Ok(ResponseResult::Ok {})));
        assert!(!has_in_flight(&commands));
    }

    #[test]
    fn large_selection_response_reassembles_without_truncation() {
        let mut commands = commands_with_in_flight();
        let selection = "selected".repeat(160_000);
        let response = serde_json::to_vec(&SuccessResponse {
            id: "request-a".into(),
            result: ResponseResult::PaneSelection {
                pane_id: "w1:p1".into(),
                text: selection.clone(),
            },
        })
        .expect("test precondition");
        let chunk_count = response.len().div_ceil(128 * 1024);
        let mut completed = None;
        for (index, chunk) in response.chunks(128 * 1024).enumerate() {
            completed = commands.receive_chunk(
                &endpoint(),
                1,
                &boot_a(),
                &request_a(),
                index + 1 == chunk_count,
                chunk.to_vec(),
            );
        }

        assert!(matches!(
            completed.expect("final selection response").result,
            Ok(ResponseResult::PaneSelection { text, .. }) if text == selection
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
            Err(ClientShellEndpointError {
                code: Some(code),
                ..
            }) if code == "endpoint_timeout"
        ));
        assert!(!has_in_flight(&commands));
        assert!(commands.expire(start + ENDPOINT_COMMAND_TIMEOUT).is_empty());
        let late_response = serde_json::to_vec(&SuccessResponse {
            id: "request-a".into(),
            result: ResponseResult::Ok {},
        })
        .expect("test precondition");
        assert!(
            commands
                .receive_chunk(&endpoint(), 1, &boot_a(), &request_a(), true, late_response)
                .is_none()
        );
        assert!(!has_in_flight(&commands));
    }

    #[test]
    fn endpoint_lanes_complete_independently() {
        let remote = ClientEndpointId::Ssh(
            crate::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef")
                .expect("test precondition"),
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
                    response: Vec::new(),
                    sent_at: Instant::now(),
                }),
                ..EndpointCommandLane::default()
            },
        );
        let response = serde_json::to_vec(&SuccessResponse {
            id: "request-b".into(),
            result: ResponseResult::Ok {},
        })
        .expect("test precondition");

        let completed = commands
            .receive_chunk(&remote, 2, &boot_b(), &"request-b".into(), true, response)
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
            crate::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef")
                .expect("test precondition"),
        );
        let mut commands = commands_with_in_flight();
        commands
            .lanes
            .get_mut(&endpoint())
            .expect("test precondition")
            .queued
            .push_back(QueuedCommand {
                generation: shepr_protocol::ConnectionGeneration::new(1),
                boot_id: boot_a(),
                request: Box::new(Request {
                    id: "queued-source".into(),
                    method: shepr_api::schema::Method::SessionSnapshot(
                        shepr_api::schema::EmptyParams::default(),
                    ),
                }),
            });
        commands.lanes.insert(
            remote.clone(),
            EndpointCommandLane {
                queued: VecDeque::from([QueuedCommand {
                    generation: shepr_protocol::ConnectionGeneration::new(2),
                    boot_id: boot_b(),
                    request: Box::new(Request {
                        id: "request-b".into(),
                        method: shepr_api::schema::Method::SessionSnapshot(
                            shepr_api::schema::EmptyParams::default(),
                        ),
                    }),
                }]),
                ..EndpointCommandLane::default()
            },
        );

        assert_eq!(
            commands.retire_lane(&endpoint()),
            vec!["request-a", "queued-source"]
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

        let late_response = serde_json::to_vec(&SuccessResponse {
            id: "request-a".into(),
            result: ResponseResult::Ok {},
        })
        .expect("test precondition");
        assert!(
            commands
                .receive_chunk(&endpoint(), 1, &boot_a(), &request_a(), true, late_response)
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
            .push_back(QueuedCommand {
                generation: shepr_protocol::ConnectionGeneration::new(1),
                boot_id: boot_a(),
                request: Box::new(Request {
                    id: "queued-a".into(),
                    method: shepr_api::schema::Method::SessionSnapshot(
                        shepr_api::schema::EmptyParams::default(),
                    ),
                }),
            });
        assert_eq!(
            commands.disconnect(&endpoint()),
            vec!["queued-a", "request-a"]
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
            crate::endpoint::ProfileId::parse("0123456789abcdef0123456789abcdef")
                .expect("test precondition"),
        );

        assert!(
            commands
                .receive_chunk(
                    &endpoint(),
                    2,
                    &boot_a(),
                    &request_a(),
                    true,
                    b"{}".to_vec()
                )
                .is_none()
        );
        assert!(
            commands
                .receive_chunk(
                    &endpoint(),
                    1,
                    &boot_b(),
                    &request_a(),
                    true,
                    b"{}".to_vec()
                )
                .is_none()
        );
        assert!(
            commands
                .receive_chunk(&unknown, 1, &boot_a(), &request_a(), true, b"{}".to_vec())
                .is_none()
        );
        assert!(has_in_flight(&commands));
    }

    #[test]
    fn oversized_response_fails_the_command_and_drops_its_remaining_chunks() {
        let mut commands = commands_with_in_flight();
        let chunk = vec![b' '; MAX_ENDPOINT_RESPONSE_BYTES / 2];
        assert!(
            commands
                .receive_chunk(
                    &endpoint(),
                    1,
                    &boot_a(),
                    &request_a(),
                    false,
                    chunk.clone()
                )
                .is_none()
        );
        assert!(
            commands
                .receive_chunk(
                    &endpoint(),
                    1,
                    &boot_a(),
                    &request_a(),
                    false,
                    chunk.clone()
                )
                .is_none()
        );
        let failed = commands
            .receive_chunk(&endpoint(), 1, &boot_a(), &request_a(), false, vec![b' '])
            .expect("oversized response completes the command");
        assert!(matches!(
            failed.result,
            Err(ClientShellEndpointError { code: Some(code), .. })
                if code == "endpoint_response_too_large"
        ));
        assert!(!has_in_flight(&commands));
        assert!(
            commands
                .receive_chunk(&endpoint(), 1, &boot_a(), &request_a(), true, b"}".to_vec())
                .is_none()
        );
    }
}
