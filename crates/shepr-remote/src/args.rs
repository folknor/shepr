use shepr_launch::invocation::{
    COMMAND_CLIENT, COMMAND_REMOTE_CLIENT_BRIDGE, COMMAND_SERVER, COMMAND_STATUS, COMMAND_STOP,
    FLAG_EXPECT_BOOT, FLAG_JSON,
};

use crate::limits::REMOTE_COMMAND_ARGS_INITIAL_CAPACITY;

/// A `shepr` command line that shepr builds for another `shepr` process, on a
/// configured machine, to parse. The words come from the invocation grammar
/// the CLI parser also spells its commands from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteCliCommand<'a> {
    ClientStatus,
    ServerStatus,
    ClientBridge,
    /// Stops only the server whose status reported this boot identity; a
    /// server of another boot refuses and keeps running, and the remote
    /// command then exits with `shepr_launch::stop::ServerStopExit::BootMismatch`.
    /// With no server running it exits with `ServerStopExit::NoServer`.
    ServerStop {
        expected_boot: &'a str,
    },
}

impl<'a> RemoteCliCommand<'a> {
    /// The argv words after the executable name.
    pub fn args(self) -> Vec<&'a str> {
        let mut args = Vec::with_capacity(REMOTE_COMMAND_ARGS_INITIAL_CAPACITY);
        match self {
            Self::ClientStatus => args.extend([COMMAND_STATUS, COMMAND_CLIENT, FLAG_JSON]),
            Self::ServerStatus => {
                args.extend([COMMAND_STATUS, COMMAND_SERVER, FLAG_JSON]);
            }
            Self::ClientBridge => args.push(COMMAND_REMOTE_CLIENT_BRIDGE),
            Self::ServerStop { expected_boot } => {
                args.extend([COMMAND_STOP, FLAG_EXPECT_BOOT, expected_boot]);
            }
        }
        args
    }
}
