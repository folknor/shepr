use crate::machine::SshTarget;

/// The local executable's default program name and CLI parser name.
pub const PROGRAM_NAME: &str = "shepr";

/// The executable name installed on remote hosts, which discovery searches for.
pub const REMOTE_INSTALL_NAME: &str = "shepr";

pub const FLAG_SESSION: &str = "--session";
pub const FLAG_REMOTE: &str = "--remote";
pub const FLAG_REMOTE_KEYBINDINGS: &str = "--remote-keybindings";
pub const FLAG_JSON: &str = "--json";
pub const FLAG_CHECK: &str = "--check";

pub fn option_name_from_flag(flag: &'static str) -> &'static str {
    flag.strip_prefix("--").unwrap_or(flag)
}

pub const COMMAND_STATUS: &str = "status";
pub const COMMAND_SERVER: &str = "server";
pub const COMMAND_CLIENT: &str = "client";
pub const COMMAND_STOP: &str = "stop";
pub const COMMAND_REMOTE_CLIENT_BRIDGE: &str = "remote-client-bridge";
pub const COMMAND_REMOTE_API_BRIDGE: &str = "remote-api-bridge";

pub const KEYBINDINGS_LOCAL: &str = "local";
pub const KEYBINDINGS_SERVER: &str = COMMAND_SERVER;

/// A `shepr` command line that shepr builds for another `shepr` process to parse.
///
/// Commands run on a remote host name their session only when it is not the
/// default one; the remote process then resolves the default by the flag's
/// absence. A local `Attach` command spells any supplied session, including
/// the default, so the caller's `SHEPR_SESSION` cannot retarget reattachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteCliCommand<'a> {
    ClientStatus,
    ServerStatus {
        session: &'a str,
    },
    ClientBridge {
        session: &'a str,
    },
    ApiBridge {
        session: &'a str,
        check: bool,
    },
    ServerStop {
        session: &'a str,
        force: bool,
    },
    /// `shepr --remote`, run locally by the operator. `session: None` leaves
    /// the session to that invocation's own resolution (which honours
    /// `SHEPR_SESSION`, as a named session's panes export it); `Some` always
    /// names it, the default session included.
    Attach {
        target: &'a str,
        session: Option<&'a str>,
        keybindings: Option<RemoteKeybindings>,
    },
}

impl<'a> RemoteCliCommand<'a> {
    /// The argv words after the executable name.
    pub fn args(self) -> Vec<&'a str> {
        let mut args = Vec::with_capacity(6);
        // Attach places its session after the target instead; client status has none.
        let session = match self {
            Self::ServerStatus { session }
            | Self::ClientBridge { session }
            | Self::ApiBridge { session, .. }
            | Self::ServerStop { session, .. } => Some(session),
            Self::ClientStatus | Self::Attach { .. } => None,
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
            Self::ApiBridge { check, .. } => {
                args.push(COMMAND_REMOTE_API_BRIDGE);
                if check {
                    args.push(FLAG_CHECK);
                }
            }
            Self::ServerStop { force, .. } => {
                args.extend([COMMAND_SERVER, COMMAND_STOP]);
                if force {
                    args.push(shepr_api::session::FORCE_STOP_FLAG);
                }
            }
            Self::Attach {
                target,
                session,
                keybindings,
            } => {
                args.extend([FLAG_REMOTE, target]);
                if let Some(keybindings) = keybindings {
                    args.extend([FLAG_REMOTE_KEYBINDINGS, keybindings.to_env_value()]);
                }
                if let Some(session) = session {
                    args.extend([FLAG_SESSION, session]);
                }
            }
        }
        args
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteKeybindings {
    Local,
    Server,
}

impl RemoteKeybindings {
    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value {
            KEYBINDINGS_LOCAL => Ok(Self::Local),
            KEYBINDINGS_SERVER => Ok(Self::Server),
            _ => Err(format!(
                "{FLAG_REMOTE_KEYBINDINGS} must be '{KEYBINDINGS_LOCAL}' or '{KEYBINDINGS_SERVER}'"
            )),
        }
    }

    pub fn from_env() -> Result<Option<Self>, String> {
        let var = shepr_core::env::EnvVar::SheprRemoteKeybindings;
        match shepr_core::env::read_text(var) {
            Ok(Some(value)) => Self::parse(&value)
                .map(Some)
                .map_err(|_| format!("{var} must be 'local' or 'server', got {value:?}")),
            Ok(None) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    /// The value both the `--remote-keybindings` flag and the environment
    /// variable carry.
    pub fn to_env_value(self) -> &'static str {
        match self {
            Self::Local => KEYBINDINGS_LOCAL,
            Self::Server => KEYBINDINGS_SERVER,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteLaunch {
    pub target: SshTarget,
    pub keybindings: RemoteKeybindings,
}

/// Builds the remote launch from the parsed `--remote` and
/// `--remote-keybindings` options. The command-line parser (`cli/spec.rs`)
/// only accepts them before the subcommand, rejects repeats, and requires
/// `--remote` for `--remote-keybindings`; the values are validated here.
pub fn remote_launch(
    target: Option<&str>,
    keybindings: Option<&str>,
) -> Result<Option<RemoteLaunch>, String> {
    let keybindings = keybindings
        .map(RemoteKeybindings::parse)
        .transpose()?
        .unwrap_or(RemoteKeybindings::Local);
    let Some(target) = target else {
        return Ok(None);
    };
    Ok(Some(RemoteLaunch {
        target: SshTarget::parse(target.to_owned()).map_err(|error| error.to_string())?,
        keybindings,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_core::env::EnvVar;
    use shepr_test_support::IsolatedEnv;

    #[test]
    fn remote_keybindings_environment_round_trips() {
        let env = IsolatedEnv::new();
        env.remove(EnvVar::SheprRemoteKeybindings);
        assert_eq!(RemoteKeybindings::from_env().expect("unset env"), None);

        for keybindings in [RemoteKeybindings::Local, RemoteKeybindings::Server] {
            env.set(EnvVar::SheprRemoteKeybindings, keybindings.to_env_value());
            assert_eq!(
                RemoteKeybindings::from_env().expect("valid env"),
                Some(keybindings)
            );
        }
    }

    #[test]
    fn remote_launch_defaults_to_local_keybindings() {
        assert_eq!(remote_launch(None, None), Ok(None));
        assert_eq!(
            remote_launch(Some("dev@box"), None),
            Ok(Some(RemoteLaunch {
                target: SshTarget::parse("dev@box").expect("test precondition"),
                keybindings: RemoteKeybindings::Local,
            }))
        );
        assert_eq!(
            remote_launch(Some("dev@box"), Some("server"))
                .expect("test precondition")
                .map(|launch| launch.keybindings),
            Some(RemoteKeybindings::Server)
        );
    }

    #[test]
    fn remote_launch_rejects_invalid_options_and_targets() {
        assert!(remote_launch(Some("-oProxyCommand=x"), None).is_err());
        assert!(remote_launch(Some(""), None).is_err());
        assert!(remote_launch(Some("dev@box"), Some("both")).is_err());
    }
}
