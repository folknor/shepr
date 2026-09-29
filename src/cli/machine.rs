use clap::ArgMatches;

use super::CliError;
use super::matches::required;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Reconnect { label: String },
}

pub(super) fn parse(matches: &ArgMatches) -> Option<Command> {
    match matches.subcommand() {
        Some(("reconnect", command)) => Some(Command::Reconnect {
            label: required(command, "label")?,
        }),
        _ => None,
    }
}

pub(super) fn run_machine_command(
    command: Command,
    context: &super::target::CliContext,
) -> super::CliResult<i32> {
    let paths: &shepr_config::AppPaths = context;
    match command {
        Command::Reconnect { label } => reconnect(paths, &label),
    }
}

/// Authenticates one configured machine in this terminal. The machine is
/// looked up by label among the `[[machines]]` of the config validated for
/// this invocation.
fn reconnect(paths: &shepr_config::AppPaths, label: &str) -> super::CliResult<i32> {
    use std::io::IsTerminal;
    let config = super::load_validated_config(paths)?;
    let machine = config
        .machines()
        .iter()
        .find(|machine| machine.label.as_str() == label)
        .ok_or_else(|| {
            CliError::Usage(format!(
                "unknown machine '{label}'; machines are the [[machines]] entries of config.toml"
            ))
        })?;
    if !std::io::stdin().is_terminal() {
        return Err(CliError::Usage(
            "reconnect requires an interactive terminal".into(),
        ));
    }
    let settings = shepr_remote::SavedSshSettings {
        manage_ssh_config: config.remote().manage_ssh_config,
    };
    let target = &machine.ssh;
    let mut authentication = shepr_remote::ssh_authentication_command(paths, target, settings)?;
    if !authentication.command.status()?.success() {
        return Err(CliError::Failed {
            message: "SSH authentication failed.".to_owned(),
            hints: Vec::new(),
        });
    }
    shepr_remote::check_saved_ssh(paths, &machine.label, target, settings)?;
    println!(
        "Machine {label} is reachable. Open Shepr clients retry within {} seconds.",
        shepr_client::endpoint::MAX_RETRY_DELAY.as_secs()
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_machine_group_only_exposes_reconnect() {
        let spec = super::super::spec::command();
        let machine = spec
            .get_subcommands()
            .find(|command| command.get_name() == "machine")
            .expect("machine command should be present");
        let subcommands = machine
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect::<Vec<_>>();

        assert_eq!(subcommands, vec!["reconnect"]);
    }

    #[test]
    fn reconnect_takes_a_label() {
        let matches = super::super::spec::command()
            .try_get_matches_from(["shepr", "machine", "reconnect", "Build"])
            .expect("test precondition");
        let Some(("machine", machine)) = matches.subcommand() else {
            panic!("machine command did not parse");
        };
        assert_eq!(
            super::parse(machine),
            Some(super::Command::Reconnect {
                label: "Build".into()
            })
        );
    }
}
