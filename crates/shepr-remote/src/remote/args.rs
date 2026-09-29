use crate::limits::REMOTE_COMMAND_ARGS_INITIAL_CAPACITY;

/// The local executable's default program name and CLI parser name.
pub const PROGRAM_NAME: &str = "shepr";

/// The executable name installed on remote hosts, which discovery searches for.
pub const REMOTE_INSTALL_NAME: &str = "shepr";

pub const FLAG_SESSION: &str = "--session";
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
///
/// Commands run on a remote host name their session only when it is not the
/// default one; the remote process then resolves the default by the flag's
/// absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteCliCommand<'a> {
    ClientStatus,
    ServerStatus { session: &'a str },
    ClientBridge { session: &'a str },
    ServerStop { session: &'a str, force: bool },
}

impl<'a> RemoteCliCommand<'a> {
    /// The argv words after the executable name.
    pub fn args(self) -> Vec<&'a str> {
        let mut args = Vec::with_capacity(REMOTE_COMMAND_ARGS_INITIAL_CAPACITY);
        // Client status names no session.
        let session = match self {
            Self::ServerStatus { session }
            | Self::ClientBridge { session }
            | Self::ServerStop { session, .. } => Some(session),
            Self::ClientStatus => None,
        };
        if let Some(session) = session
            && session != shepr_config::DEFAULT_SESSION_NAME
        {
            args.extend([FLAG_SESSION, session]);
        }

        match self {
            Self::ClientStatus => args.extend([COMMAND_STATUS, COMMAND_CLIENT, FLAG_JSON]),
            Self::ServerStatus { .. } => {
                args.extend([COMMAND_STATUS, COMMAND_SERVER, FLAG_JSON]);
            }
            Self::ClientBridge { .. } => args.push(COMMAND_REMOTE_CLIENT_BRIDGE),
            Self::ServerStop { force, .. } => {
                args.extend([COMMAND_SERVER, COMMAND_STOP]);
                if force {
                    args.push(shepr_api::session::FORCE_STOP_FLAG);
                }
            }
        }
        args
    }
}
