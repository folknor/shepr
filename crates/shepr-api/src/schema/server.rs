use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PingParams {}

/// Params of an unconditional `server.stop`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ServerStopParams {}

/// Params of `server.summary`, the session counts `shepr status` shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ServerSummaryParams {}

/// Params of `server.stop_if_boot`, the cross-build conditional stop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStopIfBootParams {
    /// Stop only the server process whose `ping` reported this boot identity.
    /// A server of any other boot refuses with `server_boot_mismatch` and keeps
    /// running. Malformed identities are rejected while decoding the request.
    pub expected_boot_id: shepr_protocol::BootId,
}

/// JSON emitted by `shepr status client --json`, also read during discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ClientStatusFields", into = "ClientStatusFields")]
pub struct ClientStatusJson {
    pub identity: Option<shepr_protocol::BuildVersion>,
    pub binary: Option<String>,
    pub server: Option<SiblingServerJson>,
}

#[derive(Serialize, Deserialize)]
struct ClientStatusFields {
    version: Option<String>,
    build_id: Option<shepr_protocol::BuildIdentity>,
    binary: Option<String>,
    server: Option<SiblingServerJson>,
}

fn build_identity(
    version: Option<String>,
    build_id: Option<shepr_protocol::BuildIdentity>,
) -> Result<Option<shepr_protocol::BuildVersion>, &'static str> {
    match (version, build_id) {
        (Some(version), Some(build_id)) => {
            Ok(Some(shepr_protocol::BuildVersion { version, build_id }))
        }
        (None, None) => Ok(None),
        _ => Err("partial build identity"),
    }
}

impl TryFrom<ClientStatusFields> for ClientStatusJson {
    type Error = &'static str;

    fn try_from(fields: ClientStatusFields) -> Result<Self, Self::Error> {
        Ok(Self {
            identity: build_identity(fields.version, fields.build_id)?,
            binary: fields.binary,
            server: fields.server,
        })
    }
}

impl From<ClientStatusJson> for ClientStatusFields {
    fn from(status: ClientStatusJson) -> Self {
        Self {
            version: status
                .identity
                .as_ref()
                .map(|identity| identity.version.clone()),
            build_id: status.identity.map(|identity| identity.build_id),
            binary: status.binary,
            server: status.server,
        }
    }
}

/// Identity of the sibling executable, or the reason it could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SiblingServerFields", into = "SiblingServerFields")]
pub struct SiblingServerJson {
    pub binary: Option<String>,
    pub identity: Result<shepr_protocol::BuildVersion, String>,
}

#[derive(Serialize, Deserialize)]
struct SiblingServerFields {
    binary: Option<String>,
    version: Option<String>,
    build_id: Option<shepr_protocol::BuildIdentity>,
    error: Option<String>,
}

impl TryFrom<SiblingServerFields> for SiblingServerJson {
    type Error = &'static str;

    fn try_from(fields: SiblingServerFields) -> Result<Self, Self::Error> {
        let identity = match (
            build_identity(fields.version, fields.build_id)?,
            fields.error,
        ) {
            (Some(identity), None) => Ok(identity),
            (None, Some(error)) => Err(error),
            _ => return Err("sibling identity and error disagree"),
        };
        Ok(Self {
            binary: fields.binary,
            identity,
        })
    }
}

impl From<SiblingServerJson> for SiblingServerFields {
    fn from(status: SiblingServerJson) -> Self {
        match status.identity {
            Ok(identity) => Self {
                binary: status.binary,
                version: Some(identity.version),
                build_id: Some(identity.build_id),
                error: None,
            },
            Err(error) => Self {
                binary: status.binary,
                version: None,
                build_id: None,
                error: Some(error),
            },
        }
    }
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

/// An answering server's complete identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerIdentity {
    pub version: String,
    pub build_id: shepr_protocol::BuildIdentity,
    pub boot_id: shepr_protocol::BootId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerStatus {
    Gone,
    Starting(ServerIdentity),
    Running(ServerIdentity),
    Stopping(ServerIdentity),
    Unresponsive,
}

/// JSON emitted by `shepr status server --json`. The state always carries
/// exactly the identity its presence requires; serde preserves the flat JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ServerStatusFields", into = "ServerStatusFields")]
pub struct ServerStatusJson {
    pub state: ServerStatus,
    pub socket: String,
}

impl ServerStatusJson {
    pub fn presence(&self) -> ServerPresenceJson {
        match self.state {
            ServerStatus::Gone => ServerPresenceJson::Gone,
            ServerStatus::Starting(_) => ServerPresenceJson::Starting,
            ServerStatus::Running(_) => ServerPresenceJson::Running,
            ServerStatus::Stopping(_) => ServerPresenceJson::Stopping,
            ServerStatus::Unresponsive => ServerPresenceJson::Unresponsive,
        }
    }

    pub fn identity(&self) -> Option<&ServerIdentity> {
        match &self.state {
            ServerStatus::Starting(identity)
            | ServerStatus::Running(identity)
            | ServerStatus::Stopping(identity) => Some(identity),
            ServerStatus::Gone | ServerStatus::Unresponsive => None,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct ServerStatusFields {
    presence: ServerPresenceJson,
    version: Option<String>,
    build_id: Option<shepr_protocol::BuildIdentity>,
    boot_id: Option<shepr_protocol::BootId>,
    socket: String,
}

impl TryFrom<ServerStatusFields> for ServerStatusJson {
    type Error = &'static str;

    fn try_from(fields: ServerStatusFields) -> Result<Self, Self::Error> {
        let identity = match (fields.version, fields.build_id, fields.boot_id) {
            (Some(version), Some(build_id), Some(boot_id)) => Some(ServerIdentity {
                version,
                build_id,
                boot_id,
            }),
            (None, None, None) => None,
            _ => return Err("partial server identity"),
        };
        let state = match (fields.presence, identity) {
            (ServerPresenceJson::Gone, None) => ServerStatus::Gone,
            (ServerPresenceJson::Unresponsive, None) => ServerStatus::Unresponsive,
            (ServerPresenceJson::Starting, Some(identity)) => ServerStatus::Starting(identity),
            (ServerPresenceJson::Running, Some(identity)) => ServerStatus::Running(identity),
            (ServerPresenceJson::Stopping, Some(identity)) => ServerStatus::Stopping(identity),
            _ => return Err("server presence and identity disagree"),
        };
        Ok(Self {
            state,
            socket: fields.socket,
        })
    }
}

impl From<ServerStatusJson> for ServerStatusFields {
    fn from(status: ServerStatusJson) -> Self {
        Self {
            presence: status.presence(),
            version: status.identity().map(|identity| identity.version.clone()),
            build_id: status.identity().map(|identity| identity.build_id),
            boot_id: status.identity().map(|identity| identity.boot_id.clone()),
            socket: status.socket,
        }
    }
}

/// The session counts a server of the reporting build answered `server.summary`
/// with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSummaryJson {
    pub workspaces: usize,
    pub panes: usize,
    pub agents: usize,
    pub blocked_agents: usize,
}

/// JSON emitted by `shepr status --json`, which `shepr status --all` also
/// reads from every configured machine's own `shepr`, whatever its build.
/// `summary` is present only when that host's server is of the reporting
/// build, is running, and answered; a build that predates the field omits it,
/// and it reads as absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusOverviewJson {
    pub local_client: ClientStatusJson,
    pub server: ServerStatusJson,
    #[serde(default)]
    pub summary: Option<ServerSummaryJson>,
}

#[cfg(test)]
mod status_tests {
    use super::*;

    #[test]
    fn an_overview_without_counts_reads_as_one_without_a_summary() {
        let value = serde_json::json!({
            "local_client": {"version": "1.0", "build_id": "0123456789abcdef", "binary": null, "server": null},
            "server": {"presence": "gone", "version": null, "build_id": null, "boot_id": null, "socket": "s"},
        });
        let overview: StatusOverviewJson =
            serde_json::from_value(value).expect("an older overview reads");
        assert_eq!(overview.summary, None);
        assert_eq!(overview.server.presence(), ServerPresenceJson::Gone);
    }

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

    #[test]
    fn client_and_sibling_status_refuse_partial_or_conflicting_identities() {
        for value in [
            serde_json::json!({"version":"1.0", "build_id":null}),
            serde_json::json!({"version":null, "build_id":"0123456789abcdef"}),
            serde_json::json!({"version":"1.0", "build_id":"garbled"}),
        ] {
            assert!(serde_json::from_value::<ClientStatusJson>(value.clone()).is_err());
            assert!(serde_json::from_value::<SiblingServerJson>(value).is_err());
        }
        let conflicting = serde_json::json!({
            "version":"1.0", "build_id":"0123456789abcdef", "error":"missing"
        });
        assert!(serde_json::from_value::<SiblingServerJson>(conflicting).is_err());
        assert!(serde_json::from_value::<SiblingServerJson>(serde_json::json!({})).is_err());
    }

    #[test]
    fn status_states_preserve_the_flat_json_shape() {
        let identity = ServerIdentity {
            version: "1.0".into(),
            build_id: "0123456789abcdef".parse().expect("build identity"),
            boot_id: "17-23".parse().expect("boot identity"),
        };
        for (state, presence, answered) in [
            (ServerStatus::Gone, "gone", false),
            (ServerStatus::Unresponsive, "unresponsive", false),
            (ServerStatus::Starting(identity.clone()), "starting", true),
            (ServerStatus::Running(identity.clone()), "running", true),
            (ServerStatus::Stopping(identity), "stopping", true),
        ] {
            let status = ServerStatusJson {
                state,
                socket: "server.sock".into(),
            };
            let expected = if answered {
                serde_json::json!({"presence":presence, "version":"1.0", "build_id":"0123456789abcdef", "boot_id":"17-23", "socket":"server.sock"})
            } else {
                serde_json::json!({"presence":presence, "version":null, "build_id":null, "boot_id":null, "socket":"server.sock"})
            };
            let encoded = serde_json::to_value(&status).expect("encode status");
            assert_eq!(encoded, expected);
            assert_eq!(
                serde_json::from_value::<ServerStatusJson>(encoded).expect("decode status"),
                status
            );
        }
        for text in [
            r#"{"version":"1.0","build_id":"0123456789abcdef","binary":null,"server":null}"#,
            r#"{"version":null,"build_id":null,"binary":null,"server":null}"#,
        ] {
            let status: ClientStatusJson = serde_json::from_str(text).expect("client status");
            assert_eq!(
                serde_json::to_value(status).expect("encode client"),
                serde_json::from_str::<serde_json::Value>(text).expect("expected JSON")
            );
        }
        for text in [
            r#"{"binary":null,"version":"1.0","build_id":"0123456789abcdef","error":null}"#,
            r#"{"binary":null,"version":null,"build_id":null,"error":"missing"}"#,
        ] {
            let status: SiblingServerJson = serde_json::from_str(text).expect("sibling status");
            assert_eq!(
                serde_json::to_value(status).expect("encode sibling"),
                serde_json::from_str::<serde_json::Value>(text).expect("expected JSON")
            );
        }
    }
}
