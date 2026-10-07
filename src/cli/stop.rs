use std::io::Write;

use shepr_launch::invocation::{FLAG_ALL, FLAG_EXPECT_BOOT, option_name_from_flag};
use shepr_launch::stop::ServerStopError;
use shepr_remote::fleet::MachineStop;

/// `shepr stop`: stops the server. With `expected_boot` (the hidden
/// `--expect-boot`, which shepr passes over SSH) only the server of that boot
/// is stopped. With `all`, every configured machine's server is stopped too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Command {
    pub(crate) expected_boot: Option<shepr_protocol::BootId>,
    pub(crate) all: bool,
}

pub(super) fn parse(matches: &clap::ArgMatches) -> Option<Command> {
    let expected_boot = super::matches::try_value::<shepr_protocol::BootId>(
        matches,
        option_name_from_flag(FLAG_EXPECT_BOOT),
    )
    .ok()?;
    let all = super::matches::try_flag(matches, option_name_from_flag(FLAG_ALL)).ok()?;
    Some(Command { expected_boot, all })
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
    if command.all {
        return stop_everywhere(paths);
    }
    shepr_launch::stop::stop_active_server(paths, command.expected_boot.as_ref())
        .map_err(super::CliError::ServerStop)?;
    Ok(0)
}

/// How one host came out of `stop --all`.
#[derive(Debug)]
enum HostStop {
    Stopped,
    NotRunning,
    /// Another server replaced the observed one and was left running.
    Replaced,
    Failed(String),
}

impl HostStop {
    fn label(&self) -> String {
        match self {
            Self::Stopped => "stopped".to_owned(),
            Self::NotRunning => "not running".to_owned(),
            Self::Replaced => {
                "replaced by another server while stopping; that one was left running".to_owned()
            }
            Self::Failed(reason) => reason.clone(),
        }
    }

    /// Whether stopping this host completed without an outstanding problem.
    fn completed_cleanly(&self) -> bool {
        matches!(self, Self::Stopped | Self::NotRunning)
    }
}

/// `stop --all`: every configured machine's server, all at once, then this
/// host's. Remote results are flushed before stopping the local server because
/// that stop can close the pane running this command. Each remote stop names
/// the boot its status reported, so a server that replaced it is left running.
/// The local result follows if this process survives its server's shutdown.
/// Exits 0 only when every stop completed cleanly with no server left.
fn stop_everywhere(paths: &shepr_paths::AppPaths) -> super::CliResult<i32> {
    let fleet = super::fleet::load(paths)?;
    let remote = shepr_remote::fleet::on_every_machine(&fleet.machines, |machine| {
        match shepr_remote::fleet::stop_machine(paths, machine) {
            Ok(MachineStop::Stopped) => HostStop::Stopped,
            Ok(MachineStop::NotRunning) => HostStop::NotRunning,
            Ok(MachineStop::Replaced) => HostStop::Replaced,
            Err(error) => HostStop::Failed(super::fleet::failure_label(&error)),
        }
    });

    let local_label = super::fleet::local_row_label(&fleet);
    let label_width = fleet
        .machines
        .iter()
        .map(|machine| machine.label.as_str().chars().count())
        .chain(std::iter::once(local_label.chars().count()))
        .max()
        .unwrap_or(0);
    let remote_rows = fleet
        .machines
        .iter()
        .zip(&remote)
        .map(|(machine, stop)| (machine.label.to_string(), stop.label()))
        .collect::<Vec<_>>();
    // A stdout that cannot take the rows does not cancel the local stop the
    // operator asked for; the write error is returned once it has run.
    let remote_written = write_rows(&super::fleet::render_rows_with_width(
        &remote_rows,
        label_width,
    ));

    let local = local_stop(paths);
    remote_written?;
    let local_row = [(local_label, local.label())];
    write_rows(&super::fleet::render_rows_with_width(
        &local_row,
        label_width,
    ))?;

    let clean = remote
        .iter()
        .chain(std::iter::once(&local))
        .all(HostStop::completed_cleanly);
    Ok(if clean { 0 } else { 1 })
}

fn write_rows(rows: &str) -> super::CliResult<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    stdout
        .write_all(rows.as_bytes())
        .map_err(super::CliError::Io)?;
    stdout.flush().map_err(super::CliError::Io)
}

fn local_stop(paths: &shepr_paths::AppPaths) -> HostStop {
    match shepr_launch::stop::stop_active_server(paths, None) {
        Ok(_) => HostStop::Stopped,
        Err(error) => local_stop_error(error),
    }
}

fn local_stop_error(error: ServerStopError) -> HostStop {
    if error.is_not_running() {
        return HostStop::NotRunning;
    }
    match error {
        ServerStopError::FinalSaveFailed {
            message,
            stop_error: None,
        } => HostStop::Failed(format!("stopped; final save failed: {message}")),
        ServerStopError::FinalSaveUnreported {
            message,
            stop_error: None,
        } => HostStop::Failed(format!("stopped; final save unconfirmed: {message}")),
        error => HostStop::Failed(format!("stop failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_hosts_left_without_a_server_count_as_done() {
        assert!(HostStop::Stopped.completed_cleanly());
        assert!(HostStop::NotRunning.completed_cleanly());
        assert!(!HostStop::Replaced.completed_cleanly());
        assert!(!HostStop::Failed("unreachable: timed out".into()).completed_cleanly());
        assert_eq!(HostStop::NotRunning.label(), "not running");
    }

    #[test]
    fn a_failed_final_save_reports_that_the_local_server_stopped() {
        let stop = local_stop_error(ServerStopError::FinalSaveFailed {
            message: "disk full".into(),
            stop_error: None,
        });

        assert_eq!(stop.label(), "stopped; final save failed: disk full");
        assert!(!stop.completed_cleanly());
    }

    #[test]
    fn an_unconfirmed_stop_after_a_failed_save_is_still_reported_as_failed() {
        let stop = local_stop_error(ServerStopError::FinalSaveFailed {
            message: "disk full".into(),
            stop_error: Some(Box::new(ServerStopError::Protocol(
                "still answering".into(),
            ))),
        });

        assert!(stop.label().starts_with("stop failed: "));
        assert!(!stop.completed_cleanly());
    }
}
