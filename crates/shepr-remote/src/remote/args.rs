use crate::limits::REMOTE_COMMAND_ARGS_INITIAL_CAPACITY;

/// The local executable's default program name and CLI parser name.
pub const PROGRAM_NAME: &str = "shepr";

/// The executable name installed on remote hosts, which discovery searches for.
pub const REMOTE_INSTALL_NAME: &str = "shepr";

pub const FLAG_JSON: &str = "--json";

/// The hidden `server stop` option that makes the stop conditional: the named
/// server boot (from that server's status) is stopped, any other refused. It is
/// for shepr's own use over SSH, not an operator command.
pub const FLAG_EXPECT_BOOT: &str = "--expect-boot";

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
pub enum RemoteCliCommand<'a> {
    ClientStatus,
    ServerStatus,
    ClientBridge,
    /// Stops only the server whose status reported this boot identity; a
    /// server of another boot refuses and keeps running, and the remote
    /// command then exits with `shepr_api::server_stop::BOOT_MISMATCH_EXIT_CODE`.
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
                args.extend([
                    COMMAND_SERVER,
                    COMMAND_STOP,
                    FLAG_EXPECT_BOOT,
                    expected_boot,
                ]);
            }
        }
        args
    }
}
