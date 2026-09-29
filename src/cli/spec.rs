//! The clap model of the whole command line. This is the only argv parser:
//! `main` parses argv with [`command`] once, then `cli` converts command
//! matches into typed arguments before dispatching to a handler. Value
//! validation lives here as value parsers, so a bad value is a usage error
//! (exit 2) instead of a transport error.

use clap::{Arg, ArgAction, Command, ValueHint};

use shepr_remote::{
    COMMAND_CLIENT, COMMAND_REMOTE_CLIENT_BRIDGE, COMMAND_SERVER, COMMAND_STATUS, COMMAND_STOP,
    FLAG_JSON, PROGRAM_NAME, option_name_from_flag,
};

pub(super) fn command() -> Command {
    let command = Command::new(PROGRAM_NAME)
        .bin_name(PROGRAM_NAME)
        .about("terminal workspace manager for AI coding agents")
        .disable_help_flag(true)
        .disable_version_flag(true)
        .arg(help_flag())
        .arg(
            Arg::new("version")
                .short('V')
                .long("version")
                .action(ArgAction::SetTrue)
                .help("Print version and exit"),
        )
        .subcommand(status_command())
        .subcommand(server_command())
        .subcommand(detect_command())
        .subcommand(integration_command())
        .subcommand(
            Command::new(COMMAND_CLIENT)
                .hide(true)
                .about("Connect to a running server's client socket"),
        )
        .subcommand(
            Command::new(COMMAND_REMOTE_CLIENT_BRIDGE)
                .hide(true)
                .about("Relay a remote client connection over stdio"),
        );
    configure_help(command, 0)
}

fn configure_help(command: Command, depth: usize) -> Command {
    // `disable_help_flag` is a *global* clap setting: once set on the root
    // (so the root can use its own plain `help` flag instead of clap's
    // immediate-exit one, see `help_flag`), it is unconditionally OR'd into
    // every descendant's settings when the tree is built, and there is no
    // public API to unset a setting a parent set globally. So subcommands
    // can't get clap's automatic `-h`/`--help` back by calling
    // `disable_help_flag(false)` on themselves; that call is silently
    // overridden. Instead, give every non-root command its own `-h`/`--help`
    // arg with the real help action, which works fine even though the
    // automatic one stays suppressed.
    let command = if depth == 0 {
        command
    } else {
        command.arg(
            Arg::new("help")
                .short('h')
                .long("help")
                .action(ArgAction::Help)
                .help("Print help"),
        )
    };
    command
        .disable_help_subcommand(true)
        .mut_subcommands(|subcommand| configure_help(subcommand, depth + 1))
}

/// A command that only groups subcommands. Invoked bare, it prints its help
/// to stderr and exits 2.
fn group(name: &'static str) -> Command {
    Command::new(name)
        .subcommand_required(true)
        .arg_required_else_help(true)
}

fn status_command() -> Command {
    Command::new(COMMAND_STATUS)
        .about("Show local client and running server status")
        .arg(flag(option_name_from_flag(FLAG_JSON)))
        .subcommand(
            Command::new(COMMAND_SERVER)
                .about("Show running server status")
                .arg(flag(option_name_from_flag(FLAG_JSON))),
        )
        .subcommand(
            Command::new(COMMAND_CLIENT)
                .about("Show local client status")
                .arg(flag(option_name_from_flag(FLAG_JSON))),
        )
}

fn server_command() -> Command {
    // Bare `shepr server` runs the headless server, so no subcommand is required.
    Command::new(COMMAND_SERVER)
        .about("Run or control the headless server")
        .subcommand(
            Command::new(COMMAND_STOP)
                .about("Stop the running server")
                .arg(force_stop_flag()),
        )
}

fn detect_command() -> Command {
    group("detect")
        .about("Capture and explain what the agent detector sees")
        .after_help("PANE is a pane id such as w1:p1.")
        .subcommand(
            Command::new("capture")
                .about("Print the plain text the detector evaluates for a pane")
                .arg(pane_id_argument().required(true)),
        )
        .subcommand(
            Command::new("explain")
                .about("Explain which detection rule decided a pane's state")
                .override_usage(
                    "shepr detect explain <PANE> [OPTIONS]\n       shepr detect explain --file <PATH> --agent <LABEL> [OPTIONS]",
                )
                .after_help(
                    "While a hook reports the pane's full agent lifecycle, screen detection is \
                     skipped and the output says so (screen_detection_skip_reason) instead of \
                     showing rule evidence.",
                )
                .arg(pane_id_argument().required_unless_present("file").conflicts_with("file"))
                .arg(
                    path_option("file", "PATH")
                        .requires("agent")
                        .help("Evaluate a saved capture locally, without a server"),
                )
                .arg(
                    option("agent", "LABEL")
                        .requires("file")
                        .help("Agent manifest to evaluate the --file capture against"),
                )
                .arg(json_flag())
                .arg(
                    Arg::new("verbose")
                        .short('v')
                        .long("verbose")
                        .action(ArgAction::SetTrue)
                        .help("List every evaluated rule with its evidence"),
                ),
        )
}

/// A live detection target: a pane id. Malformed values are rejected by the
/// parser, before any request is sent.
fn pane_id_argument() -> Arg {
    Arg::new("pane").value_name("PANE").value_parser(pane_id)
}

fn pane_id(value: &str) -> Result<String, String> {
    value
        .parse::<shepr_protocol::PublicPaneId>()
        .map(|_| value.to_owned())
        .map_err(|_| format!("{value:?} is not a pane id (expected e.g. w1:p1)"))
}

fn integration_command() -> Command {
    group("integration")
        .about("Manage built-in agent integrations")
        .subcommand(
            Command::new("install")
                .about("Install an integration")
                .arg(integration_target_arg()),
        )
        .subcommand(
            Command::new("uninstall")
                .about("Uninstall an integration")
                .arg(integration_target_arg()),
        )
        .subcommand(
            Command::new("status")
                .about("Show integration status")
                .arg(flag("outdated-only")),
        )
}

fn integration_target_arg() -> Arg {
    Arg::new("target")
        .value_name("TARGET")
        .required(true)
        .value_parser(integration_target_values())
}

fn integration_target_values() -> Vec<&'static str> {
    let values: Vec<&'static str> = shepr_api::schema::IntegrationTarget::all()
        .map(shepr_agent::integration::integration_target_label)
        .collect();
    values
}

fn json_flag() -> Arg {
    flag(option_name_from_flag(FLAG_JSON))
}

/// `server stop` refuses a server of another build, since stopping it exits
/// its panes; this flag states that stopping it is intended.
fn force_stop_flag() -> Arg {
    let long = option_name_from_flag(shepr_api::session::FORCE_STOP_FLAG);
    Arg::new(long)
        .long(long)
        .action(ArgAction::SetTrue)
        .help("Stop the server even when it runs a different shepr build")
}

fn help_flag() -> Arg {
    Arg::new("help")
        .short('h')
        .long("help")
        .action(ArgAction::SetTrue)
        .help("Show help")
}

fn flag(name: &'static str) -> Arg {
    Arg::new(name).long(name).action(ArgAction::SetTrue)
}

fn option(name: &'static str, value_name: &'static str) -> Arg {
    Arg::new(name)
        .long(name)
        .value_name(value_name)
        .action(ArgAction::Set)
}

fn path_option(name: &'static str, value_name: &'static str) -> Arg {
    option(name, value_name).value_hint(ValueHint::AnyPath)
}

#[cfg(test)]
mod tests {
    use clap::{Arg, ArgAction, Command};

    fn command_path<'a>(cmd: &'a Command, path: &[&str]) -> &'a Command {
        let mut current = cmd;
        for name in path {
            current = current
                .get_subcommands()
                .find(|subcommand| subcommand.get_name() == *name)
                .unwrap_or_else(|| panic!("missing command path segment {name}"));
        }
        current
    }

    fn argument<'a>(cmd: &'a Command, id: &str) -> &'a Arg {
        cmd.get_arguments()
            .find(|arg| arg.get_id() == id)
            .unwrap_or_else(|| panic!("missing argument {id}"))
    }

    fn collect_subcommand_paths(
        cmd: &Command,
        path: &mut Vec<String>,
        paths: &mut Vec<Vec<String>>,
    ) {
        for subcommand in cmd.get_subcommands() {
            path.push(subcommand.get_name().to_string());
            paths.push(path.clone());
            collect_subcommand_paths(subcommand, path, paths);
            path.pop();
        }
    }

    fn collect_leaf_subcommand_paths(
        cmd: &Command,
        path: &mut Vec<String>,
        paths: &mut Vec<Vec<String>>,
    ) {
        for subcommand in cmd.get_subcommands() {
            path.push(subcommand.get_name().to_string());
            if subcommand.get_subcommands().next().is_none() {
                paths.push(path.clone());
            } else {
                collect_leaf_subcommand_paths(subcommand, path, paths);
            }
            path.pop();
        }
    }

    fn sample_value(arg: &Arg) -> String {
        arg.get_value_parser()
            .possible_values()
            .into_iter()
            .flatten()
            .next()
            .map_or_else(
                || match arg.get_id().as_str() {
                    "pane" => "w1:p1".to_string(),
                    _ => "value".to_string(),
                },
                |value| value.get_name().to_string(),
            )
    }

    fn append_argument(cmd: &Command, id: &str, args: &mut Vec<String>) {
        let arg = cmd
            .get_arguments()
            .find(|arg| arg.get_id().as_str() == id)
            .unwrap_or_else(|| panic!("missing argument {id}"));
        if matches!(
            arg.get_action(),
            ArgAction::SetTrue | ArgAction::SetFalse | ArgAction::Count
        ) {
            if let Some(long) = arg.get_long() {
                args.push(format!("--{long}"));
            } else if let Some(short) = arg.get_short() {
                args.push(format!("-{short}"));
            }
            return;
        }

        let value = sample_value(arg);
        if let Some(long) = arg.get_long() {
            args.push(format!("--{long}"));
            args.push(value);
        } else if let Some(short) = arg.get_short() {
            args.push(format!("-{short}"));
            args.push(value);
        } else {
            args.push(value);
        }
    }

    fn append_required_arguments(cmd: &Command, args: &mut Vec<String>) {
        let mut selected = Vec::new();
        for arg in cmd.get_arguments().filter(|arg| arg.is_required_set()) {
            let id = arg.get_id().as_str().to_string();
            append_argument(cmd, &id, args);
            selected.push(id);
        }
        for group in cmd.get_groups().filter(|group| group.is_required_set()) {
            if group.get_args().any(|id| {
                selected
                    .iter()
                    .any(|selected| selected.as_str() == id.as_str())
            }) {
                continue;
            }
            if let Some(id) = group.get_args().next() {
                append_argument(cmd, id.as_str(), args);
                selected.push(id.as_str().to_string());
            }
        }
    }

    fn sample_leaf_invocation(spec: &Command, path: &[String]) -> Vec<String> {
        let mut args = vec!["shepr".to_string()];
        append_required_arguments(spec, &mut args);
        let mut current = spec;
        for name in path {
            let subcommand = current
                .get_subcommands()
                .find(|subcommand| subcommand.get_name() == name.as_str())
                .unwrap_or_else(|| panic!("missing command path segment {name}"));
            args.push(name.clone());
            append_required_arguments(subcommand, &mut args);
            current = subcommand;
        }
        // `detect explain` accepts either a pane or a local file and requires
        // one through a conditional argument rule rather than an ArgGroup.
        if path.iter().map(String::as_str).eq(["detect", "explain"]) {
            args.push("w1:p1".to_string());
        }
        args
    }

    fn assert_command_descriptions(cmd: &Command, path: &mut Vec<String>) {
        if !path.is_empty() {
            assert!(
                cmd.get_about().is_some(),
                "missing completion description for {}",
                path.join(" ")
            );
        }
        for subcommand in cmd.get_subcommands() {
            path.push(subcommand.get_name().to_string());
            assert_command_descriptions(subcommand, path);
            path.pop();
        }
    }

    /// Renders what `shepr <path> <flag>` prints, via the real parser.
    fn rendered_help(path: &[String], flag: &str) -> String {
        let mut args = vec!["shepr".to_string()];
        args.extend(path.iter().cloned());
        args.push(flag.to_string());
        let error = super::command()
            .try_get_matches_from(&args)
            .expect_err("help flag should stop parsing");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::DisplayHelp,
            "help was not handled for shepr {}",
            path.join(" ")
        );
        error.render().to_string()
    }

    #[test]
    fn spec_describes_all_completion_commands() {
        let cmd = super::command();
        assert_command_descriptions(&cmd, &mut Vec::new());
    }

    #[test]
    fn spec_passes_clap_invariants() {
        super::command().debug_assert();
    }

    #[test]
    fn generated_remote_cli_arguments_parse_with_the_cli_spec() {
        use shepr_remote::RemoteCliCommand;

        use crate::cli::{CliCommand, Invocation, Launch, parse_invocation, server, status};

        // Parses what the producer emits and checks what the parser made of it,
        // so a spelling that parses into the wrong command fails too.
        fn parse(command: RemoteCliCommand) -> Invocation {
            let mut argv = vec![super::PROGRAM_NAME.to_owned()];
            argv.extend(command.args().into_iter().map(str::to_owned));
            parse_invocation(&argv)
                .unwrap_or_else(|code| panic!("{command:?} should parse, exit code {code}"))
        }

        let invocation = parse(RemoteCliCommand::ClientStatus);
        assert!(matches!(
            &invocation.launch,
            Launch::Cli(command)
                if matches!(**command, CliCommand::Status(status::Command::Client { json: true }))
        ));

        let invocation = parse(RemoteCliCommand::ServerStatus);
        assert!(matches!(
            &invocation.launch,
            Launch::Cli(command)
                if matches!(**command, CliCommand::Status(status::Command::Server { json: true }))
        ));

        let invocation = parse(RemoteCliCommand::ClientBridge);
        assert!(matches!(invocation.launch, Launch::ClientBridge));

        for force in [false, true] {
            let invocation = parse(RemoteCliCommand::ServerStop { force });
            assert!(matches!(
                &invocation.launch,
                Launch::Cli(command)
                    if matches!(
                        **command,
                        CliCommand::Server(server::Command::Stop { force: parsed })
                            if parsed == force
                    )
            ));
        }
    }

    #[test]
    fn every_cli_spec_leaf_parses_to_a_typed_command() {
        // Every leaf must reach a typed variant.
        let spec = super::command();
        let mut paths = Vec::new();
        collect_leaf_subcommand_paths(&spec, &mut Vec::new(), &mut paths);
        let launch_only = ["client", "remote-client-bridge"];
        let mut classified = 0;

        for path in paths {
            if launch_only.contains(&path[0].as_str()) {
                continue;
            }
            let argv = sample_leaf_invocation(&spec, &path);
            let matches = spec
                .clone()
                .try_get_matches_from(&argv)
                .unwrap_or_else(|error| panic!("{} should parse: {error}", path.join(" ")));
            let Some((name, command_matches)) = matches.subcommand() else {
                panic!("{} did not parse as a CLI command", path.join(" "));
            };
            assert!(
                super::super::CliCommand::from_matches(name, command_matches).is_some(),
                "{} has no typed command",
                path.join(" ")
            );
            classified += 1;
        }

        assert!(classified > 0, "the spec has no classified CLI leaves");
    }

    #[test]
    fn every_spec_subcommand_renders_short_and_long_help() {
        let mut paths = Vec::new();
        collect_subcommand_paths(&super::command(), &mut Vec::new(), &mut paths);

        for path in paths {
            for flag in ["-h", "--help"] {
                let output = rendered_help(&path, flag);
                assert!(
                    output.contains(&format!("Usage: shepr {}", path.join(" "))),
                    "unexpected help for shepr {}: {output}",
                    path.join(" ")
                );
            }
        }
    }

    #[test]
    fn spec_matches_all_integration_targets() {
        let cmd = super::command();
        let install = command_path(&cmd, &["integration", "install"]);
        let expected: Vec<String> = shepr_api::schema::IntegrationTarget::all()
            .map(shepr_agent::integration::integration_target_label)
            .map(str::to_string)
            .collect();
        assert_eq!(
            argument(install, "target")
                .get_value_parser()
                .possible_values()
                .expect("test precondition")
                .map(|value| value.get_name().to_string())
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn spec_has_only_the_kept_command_groups() {
        let cmd = super::command();
        let mut names = cmd
            .get_subcommands()
            .map(Command::get_name)
            .collect::<Vec<_>>();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "client",
                "detect",
                "integration",
                "remote-client-bridge",
                "server",
                "status",
            ]
        );
    }

    #[test]
    fn bad_values_are_usage_errors() {
        for args in [
            &["shepr", "detect", "capture", "agent-name"][..],
            &["shepr", "detect", "explain", "not-a-pane"],
            &["shepr", "detect", "explain", "--file", "screen.txt"],
        ] {
            let error = super::command()
                .try_get_matches_from(args)
                .expect_err("bad value should be rejected");
            assert_eq!(error.exit_code(), 2, "{args:?}");
        }
    }
}
