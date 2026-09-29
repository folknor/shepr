use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PingParams {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSshAgentRegisterParams {
    /// Absolute remote-host agent socket. Registration lasts until this API connection closes.
    pub socket_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerCapabilities {
    pub detached_server_daemon: bool,
    /// Supports connection-scoped `server.ssh_agent.register` on the local JSON API.
    pub ssh_agent_registration: bool,
}

/// JSON emitted by `shepr status client --json`, also read during remote discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientStatusJson {
    pub version: Option<String>,
    pub build_id: Option<String>,
    pub binary: Option<String>,
}

/// JSON emitted by `shepr status server --json`, also read by saved-machine checks.
/// The running flag is the machine-readable status; human-readable status text is
/// rendered separately by the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStatusJson {
    pub running: bool,
    pub version: Option<String>,
    pub build_id: Option<String>,
    pub capabilities: Option<ServerCapabilities>,
    pub compatible: Option<bool>,
    pub socket: String,
    pub restart_needed: bool,
}
