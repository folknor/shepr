use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PingParams {}

/// Params of an unconditional `server.stop`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ServerStopParams {}

/// Params of `server.stop_if_boot`, the cross-build conditional stop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStopIfBootParams {
    /// Stop only the server process whose `ping` reported this boot identity.
    /// A server of any other boot refuses with `server_boot_mismatch` and keeps
    /// running. Malformed identities are rejected while decoding the request.
    #[serde(deserialize_with = "deserialize_boot_id")]
    pub expected_boot_id: String,
}

fn deserialize_boot_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    value
        .parse::<shepr_protocol::BootId>()
        .map(|_| value)
        .map_err(serde::de::Error::custom)
}

/// JSON emitted by `shepr status client --json`, also read during remote discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientStatusJson {
    pub version: Option<String>,
    pub build_id: Option<String>,
    pub binary: Option<String>,
    /// The `shepr-server` installed beside this client, as that host resolved
    /// it. This JSON is read across builds during remote discovery; an absent
    /// or null value is treated as an installation without a usable server.
    pub server: Option<SiblingServerJson>,
}

/// The identity of the `shepr-server` executable beside a client, read by
/// running its `--version`. Either the identity (`version` and `build_id`) is
/// present, or `error` says why it could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiblingServerJson {
    /// The path the client resolved for the sibling, when it got that far.
    pub binary: Option<String>,
    pub version: Option<String>,
    pub build_id: Option<String>,
    /// Why the sibling is missing, not executable or unreadable.
    pub error: Option<String>,
}

/// What `shepr status server` found at the server socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerPresenceJson {
    /// No live listener at the socket.
    Gone,
    /// The server answered that it is still restoring panes.
    Starting,
    /// The server answered and accepts TUI connections.
    Running,
    /// The server answered that it is stopping; it accepts no new clients.
    Stopping,
    /// Something listens at the socket but gave no status answer.
    Unresponsive,
}

/// JSON emitted by `shepr status server --json`, also read by configured-machine checks.
/// Human-readable status text is rendered separately by the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ServerStatusFields")]
pub struct ServerStatusJson {
    pub presence: ServerPresenceJson,
    /// The identity fields are set whenever the server answered (starting,
    /// running or stopping), and null when it is gone or unresponsive.
    pub version: Option<String>,
    pub build_id: Option<String>,
    /// The answering server process's boot identity, which a conditional stop
    /// (`shepr server stop --expect-boot`) names.
    pub boot_id: Option<String>,
    pub socket: String,
}

// Status discovery crosses builds. Keep the published field names, but do not
// turn a partial answer or an identity attached to absence into a live server.
#[derive(Deserialize)]
struct ServerStatusFields {
    presence: ServerPresenceJson,
    version: Option<String>,
    build_id: Option<String>,
    boot_id: Option<String>,
    socket: String,
}

impl TryFrom<ServerStatusFields> for ServerStatusJson {
    type Error = &'static str;

    fn try_from(fields: ServerStatusFields) -> Result<Self, Self::Error> {
        let answered = matches!(
            fields.presence,
            ServerPresenceJson::Starting
                | ServerPresenceJson::Running
                | ServerPresenceJson::Stopping
        );
        let complete =
            fields.version.is_some() && fields.build_id.is_some() && fields.boot_id.is_some();
        let absent =
            fields.version.is_none() && fields.build_id.is_none() && fields.boot_id.is_none();
        if (answered && !complete) || (!answered && !absent) {
            return Err("server presence and identity disagree");
        }
        if let Some(boot_id) = &fields.boot_id {
            boot_id
                .parse::<shepr_protocol::BootId>()
                .map_err(|_| "invalid server boot identity")?;
        }
        Ok(Self {
            presence: fields.presence,
            version: fields.version,
            build_id: fields.build_id,
            boot_id: fields.boot_id,
            socket: fields.socket,
        })
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;

    #[test]
    fn status_requires_an_identity_exactly_when_the_server_answered() {
        for presence in ["starting", "running", "stopping", "gone", "unresponsive"] {
            let answered = matches!(presence, "starting" | "running" | "stopping");
            for identity in [false, true] {
                let fields = if identity {
                    serde_json::json!({"version":"1.0", "build_id":"0123456789abcdef", "boot_id":"17-23"})
                } else {
                    serde_json::json!({"version":null, "build_id":null, "boot_id":null})
                };
                let mut value = fields;
                value["presence"] = serde_json::json!(presence);
                value["socket"] = serde_json::json!("server.sock");
                assert_eq!(
                    serde_json::from_value::<ServerStatusJson>(value).is_ok(),
                    answered == identity
                );
            }
        }
        let partial = serde_json::json!({"presence":"running", "version":"1.0", "build_id":null, "boot_id":"17-23", "socket":"server.sock"});
        assert!(serde_json::from_value::<ServerStatusJson>(partial).is_err());
    }
}
