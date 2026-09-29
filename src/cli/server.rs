use shepr_remote::COMMAND_STOP;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Stop { force: bool },
}

pub(super) fn parse(matches: &clap::ArgMatches) -> Option<Command> {
    match matches.subcommand() {
        Some((COMMAND_STOP, command)) => Some(Command::Stop {
            force: super::matches::flag(command, "force"),
        }),
        _ => None,
    }
}

pub(super) fn run_server_command(
    command: Command,
    paths: &super::target::CliContext,
) -> super::CliResult<i32> {
    match command {
        Command::Stop { force } => server_stop(paths, force),
    }
}

/// Skips the per-command build check: the build-mismatch error tells the user
/// to stop the server, so this must be able to stop a server from another
/// build. It is not silent, though: a server of another build is stopped only
/// with `--force`, because stopping it ends every live pane in it.
fn server_stop(paths: &super::target::CliContext, force: bool) -> super::CliResult<i32> {
    shepr_api::session::stop_active_server(paths, force).map_err(super::CliError::ServerStop)?;
    Ok(0)
}
