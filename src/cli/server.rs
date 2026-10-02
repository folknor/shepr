use shepr_remote::COMMAND_STOP;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    /// Stops the server. With `expected_boot` (the hidden `--expect-boot`,
    /// which shepr passes over SSH) only the server of that boot is stopped.
    Stop { expected_boot: Option<String> },
}

pub(super) fn parse(matches: &clap::ArgMatches) -> Option<Command> {
    match matches.subcommand() {
        Some((COMMAND_STOP, command)) => {
            let expected_boot = super::matches::try_string(
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
    paths: &super::target::CliContext,
) -> super::CliResult<i32> {
    match command {
        Command::Stop { expected_boot } => server_stop(paths, expected_boot.as_deref()),
    }
}

/// Skips the per-command build check: the build-mismatch error tells the user
/// to stop the server, so this must be able to stop a server of another build,
/// and it does so without further ceremony. Stopping ends every live pane in the
/// server, which the operator asked for by running it. With no server running
/// it exits with `ServerStopExit::NoServer`, and a refused conditional stop
/// with `ServerStopExit::BootMismatch` (see `CliError::exit_code`).
fn server_stop(
    paths: &super::target::CliContext,
    expected_boot: Option<&str>,
) -> super::CliResult<i32> {
    shepr_api::server_stop::stop_active_server(paths, expected_boot)
        .map_err(super::CliError::ServerStop)?;
    Ok(0)
}
