use shepr_launch::invocation::{FLAG_EXPECT_BOOT, option_name_from_flag};

/// `shepr stop`: stops the server. With `expected_boot` (the hidden
/// `--expect-boot`, which shepr passes over SSH) only the server of that boot
/// is stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Command {
    pub(crate) expected_boot: Option<shepr_protocol::BootId>,
}

pub(super) fn parse(matches: &clap::ArgMatches) -> Option<Command> {
    let expected_boot = super::matches::try_value::<shepr_protocol::BootId>(
        matches,
        option_name_from_flag(FLAG_EXPECT_BOOT),
    )
    .ok()?;
    Some(Command { expected_boot })
}

/// Skips the per-command build check: the build-mismatch error tells the user
/// to stop the server, so this must be able to stop a server of another build,
/// and it does so without further ceremony. Stopping ends every live pane in the
/// server, which the operator asked for by running it. With no server running
/// it exits with `ServerStopExit::NoServer`, and a refused conditional stop
/// with `ServerStopExit::BootMismatch` (see `CliError::exit_code`).
pub(super) fn run_stop_command(
    command: &Command,
    paths: &shepr_paths::AppPaths,
) -> super::CliResult<i32> {
    shepr_launch::stop::stop_active_server(paths, command.expected_boot.as_ref())
        .map_err(super::CliError::ServerStop)?;
    Ok(0)
}
