use clap::{Arg, Command};

use super::{group, json_flag, option};

pub(super) fn command() -> Command {
    group("machine")
        .about("Manage saved SSH machines")
        .after_help(
            "Add connects to a manually installed remote Shepr and starts its server before saving.
A missing or incompatible remote Shepr binary fails with an error; install Shepr on
the remote host yourself and retry.
Changes apply automatically to open local Shepr clients.
Removing a machine leaves its remote server running.
Saved machines contain only a label and an SSH target.
SSH credentials and key material remain owned by OpenSSH.",
        )
        .subcommand(
            Command::new("list")
                .about("List saved SSH machines")
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("status")
                .about("Check saved machines without prompting for authentication")
                .arg(Arg::new("machine").value_name("LABEL_OR_ID"))
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("reconnect")
                .about("Authenticate a saved machine in this terminal and verify connectivity")
                .arg(Arg::new("machine").value_name("LABEL_OR_ID").required(true)),
        )
        .subcommand(
            Command::new("add")
                .about("Prepare the remote Shepr server and save an SSH machine")
                .arg(
                    Arg::new("ssh-target")
                        .value_name("SSH_TARGET")
                        .required(true),
                )
                .arg(
                    option("label", "LABEL")
                        .required(true)
                        .help("Set the machine label shown in the sidebar"),
                ),
        )
        .subcommand(
            Command::new("remove")
                .about("Remove a saved SSH machine")
                .arg(Arg::new("machine").value_name("LABEL_OR_ID").required(true)),
        )
}
