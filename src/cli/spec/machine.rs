use clap::{Arg, Command};

use super::group;

pub(super) fn command() -> Command {
    group("machine")
        .about("Authenticate configured SSH machines")
        .after_help(
            "Machines are the [[machines]] entries of config.toml, read once when Shepr starts.
Each has a label and an SSH target; edit the config and restart Shepr to change them.
SSH credentials and key material remain owned by OpenSSH.",
        )
        .subcommand(
            Command::new("reconnect")
                .about("Authenticate a configured machine in this terminal and verify connectivity")
                .arg(Arg::new("label").value_name("LABEL").required(true)),
        )
}
