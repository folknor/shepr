use shepr_api::schema::{EmptyParams, Method, Request};
use shepr_remote::COMMAND_STOP;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Stop { force: bool },
}

impl Command {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Stop { .. } => COMMAND_STOP,
        }
    }

    pub(super) fn can_run_on_machine(self) -> bool {
        match self {
            Self::Stop { .. } => true,
        }
    }
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

/// Both paths skip the per-command build check, like `session stop`: the
/// build-mismatch error tells the user to stop the server, so this must be
/// able to stop a server from another build. That holds for `--machine` too,
/// where an in-place upgrade of the remote binary leaves the previous server
/// running. It is not silent, though: a server of another build is stopped
/// only with `--force`, because the one a dev build reaches without
/// `--session` is the installed server with every live pane in it.
fn server_stop(paths: &super::target::CliContext, force: bool) -> super::CliResult<i32> {
    if paths.is_remote() {
        if !force {
            let client = super::target::api_client(paths)?;
            let status = super::target::server_status(paths, &client).map_err(|error| {
                super::target::remote_error(paths, super::api_client_error_to_io(error))
            })?;
            let (machine, _) = super::target::remote_identity(paths).unwrap_or_default();
            if let Err(error) = shepr_api::session::guard_mismatched_stop(
                &format!("the server on machine '{machine}'"),
                &shepr_api::session::StopTargetBuild::from_running_build(&status.build_id),
                false,
                &format!(
                    "shepr --machine {machine} server stop {}",
                    shepr_api::session::FORCE_STOP_FLAG
                ),
            ) {
                return Err(super::CliError::Session(
                    super::error::SessionCliError::Stop(error),
                ));
            }
        }
        let response = super::send_request_unchecked(
            paths,
            &Request {
                id: "cli:server:stop".into(),
                method: Method::ServerStop(EmptyParams::default()),
            },
        )?;
        return Ok(if super::print_response_error(&response)? {
            1
        } else {
            0
        });
    }

    // Reported like `session stop` and the remote refusal above.
    shepr_api::session::stop_active_server(paths, force)
        .map_err(|error| super::CliError::Session(super::error::SessionCliError::Stop(error)))?;
    Ok(0)
}
