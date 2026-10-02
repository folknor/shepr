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
pub struct ServerStatusJson {
    pub presence: ServerPresenceJson,
    /// The identity fields are set whenever the server answered (starting,
    /// running or stopping), and null when it is gone or unresponsive.
    pub version: Option<String>,
    pub build_id: Option<String>,
    /// The answering server process's boot identity, which a conditional stop
    /// (`shepr server stop --expect-boot`) names.
    pub boot_id: Option<String>,
    pub compatible: Option<bool>,
    pub socket: String,
    /// True for a starting or running server of another build. A stopping one
    /// is already going away.
    pub restart_needed: bool,
}
