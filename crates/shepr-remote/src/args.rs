use shepr_launch::invocation::{
    COMMAND_CLIENT, COMMAND_REMOTE_CLIENT_BRIDGE, COMMAND_REMOTE_WAIT_FOR_SERVER, COMMAND_SERVER,
    COMMAND_STATUS, COMMAND_STOP, FLAG_EXPECT_BOOT, FLAG_JSON, FLAG_START,
};

use crate::host::BridgeMode;
use crate::limits::REMOTE_COMMAND_ARGS_INITIAL_CAPACITY;

/// A `shepr` command line that shepr builds for another `shepr` process, on a
/// configured machine, to parse. The words come from the invocation grammar
/// the CLI parser also spells its commands from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteCliCommand<'a> {
    /// `status --json`: the host's installation, server and, for a running
    /// server of that build, its counts.
    Overview,
    ClientStatus,
    ServerStatus,
    /// The stdio bridge that only attaches to the host's running server.
    ClientBridge,
    /// The stdio bridge that starts the host's server when none runs: only
    /// for the operator's Connect or Restart.
    StartingClientBridge,
    /// Blocks until a server answers on the host, then exits. It starts
    /// nothing.
    WaitForServer,
    /// Stops only the server whose status reported this boot identity; a
    /// server of another boot refuses and keeps running, and the remote
    /// command then exits with `shepr_launch::stop::ServerStopExit::BootMismatch`.
    /// With no server running it exits with `ServerStopExit::NoServer`.
    ServerStop {
        expected_boot: &'a str,
    },
}

impl<'a> RemoteCliCommand<'a> {
    /// The bridge command for `mode`.
    pub fn client_bridge(mode: BridgeMode) -> Self {
        match mode {
            BridgeMode::Attach => Self::ClientBridge,
            BridgeMode::Start => Self::StartingClientBridge,
        }
    }

    /// The argv words after the executable name.
    pub fn args(self) -> Vec<&'a str> {
        let mut args = Vec::with_capacity(REMOTE_COMMAND_ARGS_INITIAL_CAPACITY);
        match self {
            Self::Overview => args.extend([COMMAND_STATUS, FLAG_JSON]),
            Self::ClientStatus => args.extend([COMMAND_STATUS, COMMAND_CLIENT, FLAG_JSON]),
            Self::ServerStatus => {
                args.extend([COMMAND_STATUS, COMMAND_SERVER, FLAG_JSON]);
            }
            Self::ClientBridge => args.push(COMMAND_REMOTE_CLIENT_BRIDGE),
            Self::StartingClientBridge => {
                args.extend([COMMAND_REMOTE_CLIENT_BRIDGE, FLAG_START]);
            }
            Self::WaitForServer => args.push(COMMAND_REMOTE_WAIT_FOR_SERVER),
            Self::ServerStop { expected_boot } => {
                args.extend([COMMAND_STOP, FLAG_EXPECT_BOOT, expected_boot]);
            }
        }
        args
    }
}
