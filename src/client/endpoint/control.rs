use super::ClientEndpointId;

pub(crate) enum EndpointControlMessage {
    HealthPong,
    AgentCompletions(crate::protocol::endpoint::EndpointAgentCompletions),
    Snapshot(Box<crate::protocol::ClientShellSnapshot>),
    Ignored,
}

pub(crate) fn decode_endpoint_control(
    kind: &str,
    data: &str,
) -> Result<EndpointControlMessage, String> {
    if kind == crate::protocol::endpoint::HEALTH_PONG_KIND {
        return Ok(EndpointControlMessage::HealthPong);
    }
    if kind == crate::protocol::endpoint::AGENT_COMPLETIONS_KIND {
        return Ok(serde_json::from_str(data)
            .map(EndpointControlMessage::AgentCompletions)
            .unwrap_or(EndpointControlMessage::Ignored));
    }
    if kind == crate::protocol::endpoint::ENDPOINT_SNAPSHOT_KIND {
        let snapshot = serde_json::from_str(data)
            .map_err(|error| format!("invalid endpoint snapshot: {error}"))?;
        return Ok(EndpointControlMessage::Snapshot(Box::new(snapshot)));
    }
    if kind.starts_with("shell.snapshot.") {
        return Err(format!(
            "unsupported mandatory endpoint snapshot codec {kind:?}"
        ));
    }
    Ok(EndpointControlMessage::Ignored)
}

pub(crate) fn protocol_failure_is_fatal(endpoint_id: &ClientEndpointId) -> bool {
    endpoint_id.is_local()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::endpoint::ProfileId;

    #[test]
    fn unknown_optional_controls_are_ignored() {
        assert!(matches!(
            decode_endpoint_control("future.optional", "not json").expect("test precondition"),
            EndpointControlMessage::Ignored
        ));
    }

    #[test]
    fn completion_guard_optional_control_round_trips_without_changing_snapshot_codec() {
        let projection = crate::protocol::endpoint::EndpointAgentCompletions {
            boot_id: "boot".into(),
            revision: 3,
            completions: [("pane".into(), 7)].into_iter().collect(),
        };
        let crate::protocol::ServerMessage::EndpointControl { kind, data } =
            crate::protocol::endpoint::agent_completions_message(&projection).expect("test precondition")
        else {
            panic!("expected optional control");
        };
        let EndpointControlMessage::AgentCompletions(decoded) =
            decode_endpoint_control(&kind, &data).expect("test precondition")
        else {
            panic!("expected completion projection");
        };
        assert_eq!(decoded, projection);
        assert!(matches!(
            decode_endpoint_control(&kind, "invalid").expect("test precondition"),
            EndpointControlMessage::Ignored
        ));
    }

    #[test]
    fn unknown_snapshot_codecs_are_rejected() {
        assert_eq!(
            decode_endpoint_control("shell.snapshot.v2", "{}")
                .err()
                .as_deref(),
            Some("unsupported mandatory endpoint snapshot codec \"shell.snapshot.v2\"")
        );
    }

    #[test]
    fn only_local_protocol_failures_end_the_client() {
        let remote =
            ClientEndpointId::Ssh(ProfileId::parse("0123456789abcdef0123456789abcdef").expect("test precondition"));
        assert!(protocol_failure_is_fatal(&ClientEndpointId::Local));
        assert!(!protocol_failure_is_fatal(&remote));
    }
}
