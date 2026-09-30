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
    /// running.
    pub expected_boot_id: String,
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

/// JSON emitted by `shepr status server --json`, also read by configured-machine checks.
/// The running flag is the machine-readable status; human-readable status text is
/// rendered separately by the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStatusJson {
    pub running: bool,
    pub version: Option<String>,
    pub build_id: Option<String>,
    /// The running server process's boot identity, which a conditional stop
    /// (`shepr server stop --expect-boot`) names.
    pub boot_id: Option<String>,
    pub compatible: Option<bool>,
    pub socket: String,
    pub restart_needed: bool,
}
