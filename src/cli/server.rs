use shepr_remote::COMMAND_STOP;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    /// Stops the server. With `expected_boot` (the hidden `--expect-boot`,
    /// which shepr passes over SSH) only the server of that boot is stopped.
    Stop {
        expected_boot: Option<shepr_protocol::BootId>,
    },
}

pub(super) fn parse(matches: &clap::ArgMatches) -> Option<Command> {
    match matches.subcommand() {
        Some((COMMAND_STOP, command)) => {
            let expected_boot = super::matches::try_value::<shepr_protocol::BootId>(
                command,
                shepr_remote::option_name_from_flag(shepr_remote::FLAG_EXPECT_BOOT),
            )
            .ok()?;
            Some(Command::Stop { expected_boot })
        }
        _ => None,
    }
}

pub(super) fn run_server_command(
    command: Command,
    paths: &shepr_config::AppPaths,
) -> super::CliResult<i32> {
    match command {
        Command::Stop { expected_boot } => server_stop(paths, expected_boot.as_ref()),
    }
}

/// Skips the per-command build check: the build-mismatch error tells the user
/// to stop the server, so this must be able to stop a server of another build,
/// and it does so without further ceremony. Stopping ends every live pane in the
/// server, which the operator asked for by running it. With no server running
/// it exits with `ServerStopExit::NoServer`, and a refused conditional stop
/// with `ServerStopExit::BootMismatch` (see `CliError::exit_code`).
fn server_stop(
    paths: &shepr_config::AppPaths,
    expected_boot: Option<&shepr_protocol::BootId>,
) -> super::CliResult<i32> {
    shepr_api::server_stop::stop_active_server(
        paths,
        expected_boot.map(shepr_protocol::BootId::as_str),
    )
    .map_err(super::CliError::ServerStop)?;
    Ok(0)
}
