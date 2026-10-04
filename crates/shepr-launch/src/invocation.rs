//! The command lines shepr processes build for each other and parse: the
//! server executable's arguments and its `--version` line, and the CLI's
//! program, command and option words. The CLI parser (the binary's clap spec),
//! the local launcher and the SSH command producer all spell a command from
//! these, so a producer and its parser cannot drift apart.

/// The server executable's file name, looked up beside the running client.
pub const SERVER_BINARY_NAME: &str = "shepr-server";

/// The private argument a client passes to the server executable to mark a
/// start by a shepr client.
pub const CLIENT_SPAWNED_FLAG: &str = "--client-spawned";

/// The server executable's one public argument: print the build identity the
/// client compares against, and exit.
pub const VERSION_FLAG: &str = "--version";

/// What a server executable was asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerInvocation {
    /// Run the server in the foreground; `client_spawned` marks a start by a
    /// shepr client.
    Serve { client_spawned: bool },
    /// Print [`server_version_line`] and exit.
    Version,
}

impl ServerInvocation {
    /// The arguments (after the executable name) that spell this invocation:
    /// the one place a producer takes them from, and what [`Self::parse`]
    /// reads back.
    pub fn args(self) -> &'static [&'static str] {
        match self {
            Self::Serve {
                client_spawned: false,
            } => &[],
            Self::Serve {
                client_spawned: true,
            } => &[CLIENT_SPAWNED_FLAG],
            Self::Version => &[VERSION_FLAG],
        }
    }

    /// The invocation `args` (after the executable name) spell, or `None` for
    /// any other argument list.
    pub fn parse(args: &[&str]) -> Option<Self> {
        match args {
            [] => Some(Self::Serve {
                client_spawned: false,
            }),
            [CLIENT_SPAWNED_FLAG] => Some(Self::Serve {
                client_spawned: true,
            }),
            [VERSION_FLAG] => Some(Self::Version),
            _ => None,
        }
    }
}

/// The usage line a server executable prints for arguments it does not take.
pub fn server_usage() -> String {
    format!("usage: {SERVER_BINARY_NAME} [{VERSION_FLAG}]")
}

/// The line `shepr-server --version` prints: `shepr-server <version>+<build id>`.
pub fn server_version_line() -> String {
    format!("{SERVER_BINARY_NAME} {}", shepr_protocol::build_version())
}

/// Splits a [`server_version_line`] into the version and the build id. `None`
/// for any other text.
pub fn parse_server_version_line(line: &str) -> Option<(String, shepr_protocol::BuildIdentity)> {
    let identity = line.trim().strip_prefix(SERVER_BINARY_NAME)?.trim();
    let identity = identity.parse::<shepr_protocol::BuildVersion>().ok()?;
    Some((identity.version, identity.build_id))
}

/// The local executable's default program name and CLI parser name, and the
/// command a release build's guidance names.
pub const PROGRAM_NAME: &str = "shepr";

/// The executable name installed on remote hosts, which discovery searches for.
pub const REMOTE_INSTALL_NAME: &str = "shepr";

pub const FLAG_JSON: &str = "--json";

/// The hidden `stop` option that makes the stop conditional: the named
/// server boot (from that server's status) is stopped, any other refused. It is
/// for shepr's own use over SSH, not an operator command.
pub const FLAG_EXPECT_BOOT: &str = "--expect-boot";

/// The clap argument name of a `--flag`: the flag without its dashes.
pub fn option_name_from_flag(flag: &'static str) -> &'static str {
    flag.strip_prefix("--").unwrap_or(flag)
}

pub const COMMAND_DETECT: &str = "detect";
pub const COMMAND_STATUS: &str = "status";
pub const COMMAND_SERVER: &str = "server";
pub const COMMAND_CLIENT: &str = "client";
pub const COMMAND_STOP: &str = "stop";
pub const COMMAND_MAN: &str = "man";
pub const COMMAND_REMOTE_CLIENT_BRIDGE: &str = "remote-client-bridge";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_takes_no_arguments_but_its_two_flags() {
        assert_eq!(
            ServerInvocation::parse(&[]),
            Some(ServerInvocation::Serve {
                client_spawned: false
            })
        );
        assert_eq!(
            ServerInvocation::parse(&[CLIENT_SPAWNED_FLAG]),
            Some(ServerInvocation::Serve {
                client_spawned: true
            })
        );
        assert_eq!(
            ServerInvocation::parse(&[VERSION_FLAG]),
            Some(ServerInvocation::Version)
        );
        for other in [
            &["--help"][..],
            &[VERSION_FLAG, CLIENT_SPAWNED_FLAG],
            &[CLIENT_SPAWNED_FLAG, CLIENT_SPAWNED_FLAG],
        ] {
            assert_eq!(ServerInvocation::parse(other), None, "{other:?}");
        }
        assert_eq!(server_usage(), "usage: shepr-server [--version]");
        for invocation in [
            ServerInvocation::Serve {
                client_spawned: false,
            },
            ServerInvocation::Serve {
                client_spawned: true,
            },
            ServerInvocation::Version,
        ] {
            assert_eq!(ServerInvocation::parse(invocation.args()), Some(invocation));
        }
    }

    #[test]
    fn the_version_line_round_trips() {
        let (version, build_id) =
            parse_server_version_line(&server_version_line()).expect("own version line parses");
        assert_eq!(version, shepr_protocol::PACKAGE_VERSION);
        assert!(build_id.is_this_build());
    }

    #[test]
    fn the_version_line_yields_the_version_and_build_id() {
        assert_eq!(
            parse_server_version_line("shepr-server 0.6.0+0123456789abcdef\n"),
            Some((
                "0.6.0".to_owned(),
                "0123456789abcdef".parse().expect("build identity")
            ))
        );
        for bad in [
            "",
            "shepr 0.6.0+0123456789abcdef",
            "shepr-server 0.6.0",
            "shepr-server +0123456789abcdef",
            "shepr-server 0.6.0+",
            "shepr-server 0.6.0+abc def",
        ] {
            assert_eq!(parse_server_version_line(bad), None, "{bad:?}");
        }
    }
}
