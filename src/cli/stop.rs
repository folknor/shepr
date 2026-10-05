use shepr_launch::invocation::{FLAG_ALL, FLAG_EXPECT_BOOT, option_name_from_flag};
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

    /// Whether the host ended with no server, what `stop --all` is for.
    fn ended_without_a_server(&self) -> bool {
        matches!(self, Self::Stopped | Self::NotRunning)
    }
}

/// `stop --all`: every configured machine's server, all at once, then this
/// host's. The local one stops last, so a TUI attached to it is still up
/// while the machines report. Each remote stop names the boot its status
/// reported, so a server that replaced it is left running. Exits 0 only when
/// every host ended with no server.
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
    let local = local_stop(paths);

    let mut rows = fleet
        .machines
        .iter()
        .zip(&remote)
        .map(|(machine, stop)| (machine.label.to_string(), stop.label()))
        .collect::<Vec<_>>();
    rows.push((super::fleet::local_row_label(&fleet), local.label()));
    print!("{}", super::fleet::render_rows(&rows));

    let clean = remote
        .iter()
        .chain(std::iter::once(&local))
        .all(HostStop::ended_without_a_server);
    Ok(if clean { 0 } else { 1 })
}

fn local_stop(paths: &shepr_paths::AppPaths) -> HostStop {
    match shepr_launch::stop::stop_active_server(paths, None) {
        Ok(_) => HostStop::Stopped,
        Err(error) if error.is_not_running() => HostStop::NotRunning,
        Err(error) => HostStop::Failed(format!("stop failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_hosts_left_without_a_server_count_as_done() {
        assert!(HostStop::Stopped.ended_without_a_server());
        assert!(HostStop::NotRunning.ended_without_a_server());
        assert!(!HostStop::Replaced.ended_without_a_server());
        assert!(!HostStop::Failed("unreachable: timed out".into()).ended_without_a_server());
        assert_eq!(HostStop::NotRunning.label(), "not running");
    }
}
