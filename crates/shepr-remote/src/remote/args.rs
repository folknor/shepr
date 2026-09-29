use crate::limits::REMOTE_COMMAND_ARGS_INITIAL_CAPACITY;

/// The local executable's default program name and CLI parser name.
pub const PROGRAM_NAME: &str = "shepr";

/// The executable name installed on remote hosts, which discovery searches for.
pub const REMOTE_INSTALL_NAME: &str = "shepr";

pub const FLAG_JSON: &str = "--json";

pub fn option_name_from_flag(flag: &'static str) -> &'static str {
    flag.strip_prefix("--").unwrap_or(flag)
}

pub const COMMAND_STATUS: &str = "status";
pub const COMMAND_SERVER: &str = "server";
pub const COMMAND_CLIENT: &str = "client";
pub const COMMAND_STOP: &str = "stop";
pub const COMMAND_REMOTE_CLIENT_BRIDGE: &str = "remote-client-bridge";

/// A `shepr` command line that shepr builds for another `shepr` process to parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteCliCommand {
    ClientStatus,
    ServerStatus,
    ClientBridge,
    ServerStop { force: bool },
}

impl RemoteCliCommand {
    /// The argv words after the executable name.
    pub fn args(self) -> Vec<&'static str> {
        let mut args = Vec::with_capacity(REMOTE_COMMAND_ARGS_INITIAL_CAPACITY);
        match self {
            Self::ClientStatus => args.extend([COMMAND_STATUS, COMMAND_CLIENT, FLAG_JSON]),
            Self::ServerStatus => {
                args.extend([COMMAND_STATUS, COMMAND_SERVER, FLAG_JSON]);
            }
            Self::ClientBridge => args.push(COMMAND_REMOTE_CLIENT_BRIDGE),
            Self::ServerStop { force } => {
                args.extend([COMMAND_SERVER, COMMAND_STOP]);
                if force {
                    args.push(shepr_api::session::FORCE_STOP_FLAG);
                }
            }
        }
        args
    }
}
