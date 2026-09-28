use clap::ArgMatches;
use serde::Serialize;

use shepr_remote::machine::{EndpointCatalog, SshMetadataCache, SshTarget};

use super::CliError;
use super::matches::{flag, required, string};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    List { json: bool },
    Status { machine: Option<String>, json: bool },
    Reconnect { machine: String },
    Add(AddArgs),
    Remove { machine: String },
    Invalid,
}

impl Command {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::List { .. } => "list",
            Self::Status { .. } => "status",
            Self::Reconnect { .. } => "reconnect",
            Self::Add(_) => "add",
            Self::Remove { .. } => "remove",
            Self::Invalid => "",
        }
    }
}

pub(super) fn parse(matches: &ArgMatches) -> Command {
    match matches.subcommand() {
        Some(("list", command)) => Command::List {
            json: flag(command, "json"),
        },
        Some(("status", command)) => Command::Status {
            machine: string(command, "machine"),
            json: flag(command, "json"),
        },
        Some(("reconnect", command)) => Command::Reconnect {
            machine: required(command, "machine"),
        },
        Some(("add", command)) => Command::Add(add_args(command)),
        Some(("remove", command)) => Command::Remove {
            machine: required(command, "machine"),
        },
        _ => Command::Invalid,
    }
}

#[derive(Serialize)]
struct MachineListRow<'a> {
    id: &'a str,
    label: &'a str,
    target: &'a str,
    session: &'a str,
    selected: bool,
}

pub(super) fn run_machine_command(
    command: Command,
    context: &super::target::CliContext,
) -> super::CliResult<i32> {
    let paths: &shepr_config::AppPaths = context;
    match command {
        Command::List { json } => list(paths, json),
        Command::Status { machine, json } => {
            status(machine.as_deref(), json, paths, saved_ssh_settings(paths)?)
        }
        Command::Reconnect { machine } => reconnect(paths, &machine, saved_ssh_settings(paths)?),
        Command::Add(args) => add(paths, args, saved_ssh_settings(paths)?),
        Command::Remove { machine } => remove(paths, &machine),
        Command::Invalid => Ok(super::missing_subcommand()),
    }
}

fn list(paths: &shepr_config::AppPaths, json: bool) -> super::CliResult<i32> {
    let catalog = load_catalog(paths)?;
    let selected_profile = catalog.load_selection();
    let rows = catalog
        .ssh
        .iter()
        .map(|profile| MachineListRow {
            id: profile.id.as_str(),
            label: &profile.label,
            target: profile.target.as_str(),
            session: &profile.session,
            selected: selected_profile.as_ref() == Some(&profile.id),
        })
        .collect::<Vec<_>>();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(std::io::Error::other)?
        );
        return Ok(0);
    }
    if rows.is_empty() {
        println!("No saved SSH machines.");
        return Ok(0);
    }
    for row in rows {
        println!("{}\t{}\t{}\t{}", row.id, row.label, row.target, row.session);
    }
    Ok(0)
}

#[derive(Serialize)]
struct MachineStatusRow<'a> {
    id: &'a str,
    label: &'a str,
    status: &'static str,
    error: Option<String>,
}

fn status(
    selector: Option<&str>,
    json: bool,
    paths: &shepr_config::AppPaths,
    settings: shepr_remote::SavedSshSettings,
) -> super::CliResult<i32> {
    let catalog = load_catalog(paths)?;
    let profiles = match selector {
        Some(selector) => {
            vec![super::target::resolve_machine(&catalog.ssh, selector).map_err(CliError::Usage)?]
        }
        None => catalog.ssh.iter().collect(),
    };
    let rows = profiles
        .into_iter()
        .map(|profile| {
            let (status, error) = match shepr_remote::check_saved_ssh(
                paths,
                &profile.target,
                &profile.session,
                settings,
            ) {
                Ok(()) => ("reachable", None),
                Err(error) => {
                    let status = if shepr_remote::SshFailureDiagnostic::from_error(&error)
                        .requires_authentication()
                    {
                        "auth required"
                    } else {
                        "error"
                    };
                    (status, Some(error.to_string()))
                }
            };
            MachineStatusRow {
                id: profile.id.as_str(),
                label: &profile.label,
                status,
                error,
            }
        })
        .collect::<Vec<_>>();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(std::io::Error::other)?
        );
    } else {
        for row in &rows {
            println!("{}\t{}\t{}", row.id, row.label, row.status);
            if let Some(error) = &row.error {
                println!("  {}", error.escape_debug());
            }
        }
        if rows.is_empty() {
            println!("No saved SSH machines.");
        }
    }
    Ok(i32::from(rows.iter().any(|row| row.error.is_some())))
}

fn reconnect(
    paths: &shepr_config::AppPaths,
    selector: &str,
    settings: shepr_remote::SavedSshSettings,
) -> super::CliResult<i32> {
    use std::io::IsTerminal;
    let catalog = load_catalog(paths)?;
    let profile =
        super::target::resolve_machine(&catalog.ssh, selector).map_err(CliError::Usage)?;
    if !std::io::stdin().is_terminal() {
        return Err(CliError::Usage(
            "reconnect requires an interactive terminal; use shepr machine status for noninteractive checks"
                .into(),
        ));
    }
    let mut authentication =
        shepr_remote::ssh_authentication_command(paths, &profile.target, settings)?;
    if !authentication.command.status()?.success() {
        return Err(failed(
            "SSH authentication failed; the saved machine was not changed.",
        ));
    }
    shepr_remote::check_saved_ssh(paths, &profile.target, &profile.session, settings)?;
    println!(
        "Machine {} is reachable. Open Shepr clients retry within {} seconds.",
        profile.id,
        shepr_client::endpoint::MAX_RETRY_DELAY.as_secs()
    );
    Ok(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AddArgs {
    target: String,
    label: String,
    session: String,
}

fn add_args(matches: &ArgMatches) -> AddArgs {
    AddArgs {
        target: required(matches, "ssh-target"),
        label: required(matches, "label"),
        session: string(matches, "remote-session")
            .unwrap_or_else(|| shepr_config::DEFAULT_SESSION_NAME.to_owned()),
    }
}

fn add(
    paths: &shepr_config::AppPaths,
    args: AddArgs,
    settings: shepr_remote::SavedSshSettings,
) -> super::CliResult<i32> {
    let AddArgs {
        target,
        label,
        session,
    } = args;
    let target = SshTarget::parse(target).map_err(CliError::Usage)?;
    let mut catalog = load_catalog(paths)?;
    // This preflight validates fields and capacity before remote setup can wait. Its ID is
    // intentionally discarded; IDs identify saved rows. Duplicate labels are permitted,
    // and selectors report ambiguity so callers can use the profile ID.
    catalog
        .add_ssh(label.clone(), target.clone(), session.clone())
        .map_err(CliError::Usage)?;
    let executable = shepr_remote::prepare_saved_ssh(
        paths,
        &target,
        &session,
        settings,
        &mut super::operator::TerminalOperator,
    )
    .map_err(|error| CliError::Failed {
        message: format!("{error}; machine was not saved"),
        hints: shepr_remote::saved_ssh_error_hint(&error, &target),
    })?;
    // Setup can wait for human approval. Do not overwrite catalog edits made meanwhile.
    let mut catalog = load_catalog(paths).map_err(|error| {
        std::io::Error::other(format!(
            "remote prepared, but machine was not saved: {error}"
        ))
    })?;
    let id = catalog
        .add_ssh(label, target.clone(), &session)
        .map_err(CliError::Usage)?;
    store_catalog(&mut catalog).map_err(|error| {
        std::io::Error::other(format!(
            "remote prepared, but machine was not saved: {error}"
        ))
    })?;
    println!("Saved SSH machine {id}. Remote server is ready.");
    // The machine is saved and reachable either way; a missing cache only means
    // the first connection discovers the remote shepr again, so this is a warning
    // and not a failed add.
    let metadata_cache = SshMetadataCache::new(paths, &id, &target, &session);
    if let Err(error) = metadata_cache.store(&executable) {
        eprintln!(
            "warning: could not cache the remote shepr location in {}: {error}; \
             connections to {id} rediscover it until the cache can be written",
            metadata_cache.path().display()
        );
    }
    println!("Open Shepr clients connect automatically.");
    Ok(0)
}

fn saved_ssh_settings(
    paths: &shepr_config::AppPaths,
) -> super::CliResult<shepr_remote::SavedSshSettings> {
    let config = super::load_validated_config(paths)?;
    Ok(shepr_remote::SavedSshSettings {
        manage_ssh_config: config.remote().manage_ssh_config,
    })
}

fn remove(paths: &shepr_config::AppPaths, selector: &str) -> super::CliResult<i32> {
    let mut catalog = load_catalog(paths)?;
    let profile =
        super::target::resolve_machine(&catalog.ssh, selector).map_err(CliError::Usage)?;
    let id = profile.id.clone();
    let was_selected = catalog.load_selection().as_ref() == Some(&id);
    let metadata_cache =
        SshMetadataCache::new(paths, &id, profile.target.as_str(), &profile.session);
    if !catalog.remove_ssh(&id) {
        return Err(failed(&format!("machine profile {id} was not found")));
    }
    store_catalog(&mut catalog)?;
    // The profile is already gone from the catalog and a new profile gets a fresh
    // ID, so nothing reads this file again; a leftover is an orphaned private file
    // worth naming, not a failed removal.
    if let Err(error) = metadata_cache.invalidate() {
        eprintln!(
            "warning: could not remove cached SSH metadata {}: {error}",
            metadata_cache.path().display()
        );
    }
    // The next launch falls back to Local instead of naming a removed machine.
    if was_selected {
        catalog
            .store_selection(None)
            .map_err(std::io::Error::other)?;
    }
    println!("Removed SSH machine {id}.");
    Ok(0)
}

/// A command failure reported as prose, exit status 1.
fn failed(message: &str) -> CliError {
    CliError::Failed {
        message: message.to_owned(),
        hints: Vec::new(),
    }
}

fn load_catalog(paths: &shepr_config::AppPaths) -> super::CliResult<EndpointCatalog> {
    EndpointCatalog::load(paths).map_err(|error| std::io::Error::other(error).into())
}

fn store_catalog(catalog: &mut EndpointCatalog) -> super::CliResult<()> {
    catalog
        .store_profiles()
        .map_err(|error| std::io::Error::other(error).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_add_args(args: &[String]) -> Result<AddArgs, clap::Error> {
        let mut argv = vec![
            "shepr".to_string(),
            "machine".to_string(),
            "add".to_string(),
        ];
        argv.extend_from_slice(args);
        let matches = super::super::spec::command().try_get_matches_from(&argv)?;
        let Some(("machine", machine)) = matches.subcommand() else {
            panic!("machine command did not parse");
        };
        let Some(("add", add)) = machine.subcommand() else {
            panic!("machine add did not parse");
        };
        Ok(add_args(add))
    }

    #[test]
    fn machine_mutation_commands_only_expose_add_and_remove() {
        for command in ["rename", "enable", "disable"] {
            let argv = [
                "shepr",
                "machine",
                command,
                "0123456789abcdef0123456789abcdef",
            ];
            assert!(
                super::super::spec::command()
                    .try_get_matches_from(argv)
                    .is_err(),
                "{command} must not be exposed"
            );
        }
    }

    #[test]
    fn machine_remove_takes_the_shared_label_or_id_selector() {
        let matches = super::super::spec::command()
            .try_get_matches_from(["shepr", "machine", "remove", "Build"])
            .expect("test precondition");
        let Some(("machine", machine)) = matches.subcommand() else {
            panic!("machine command did not parse");
        };
        let Some(("remove", remove)) = machine.subcommand() else {
            panic!("machine remove did not parse");
        };
        assert_eq!(
            remove.get_one::<String>("machine").map(String::as_str),
            Some("Build")
        );
    }

    #[test]
    fn add_parser_preserves_values_across_argument_orders() {
        for (args, session) in [
            (vec!["--label", "coder", "workstation.coder"], "default"),
            (vec!["workstation.coder", "--label", "coder"], "default"),
            (
                vec![
                    "--remote-session",
                    "agents",
                    "workstation.coder",
                    "--label",
                    "coder",
                ],
                "agents",
            ),
            (
                vec![
                    "--label=coder",
                    "--remote-session=agents",
                    "workstation.coder",
                ],
                "agents",
            ),
        ] {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert_eq!(
                parse_add_args(&args).expect("test precondition"),
                AddArgs {
                    target: "workstation.coder".into(),
                    label: "coder".into(),
                    session: session.into(),
                },
                "{args:?}"
            );
        }
    }

    #[test]
    fn add_parser_rejects_incomplete_duplicate_and_extra_arguments() {
        for args in [
            vec![],
            vec!["--label", "coder"],
            vec!["workstation.coder"],
            vec!["workstation.coder", "--label"],
            vec!["workstation.coder", "--label", "coder", "--remote-session"],
            vec!["--label", "coder", "--label", "other", "workstation.coder"],
            vec![
                "workstation.coder",
                "--label",
                "coder",
                "--remote-session",
                "a",
                "--remote-session",
                "b",
            ],
            vec!["--label", "coder", "workstation.coder", "other-host"],
            vec!["--unknown", "workstation.coder", "--label", "coder"],
            vec!["--label", "--remote-session", "agents", "workstation.coder"],
        ] {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(parse_add_args(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn add_label_does_not_swallow_the_next_option() {
        // `--label` needs a value; a following option is not taken as one, so
        // the missing label is what gets reported.
        let args = ["workstation.coder", "--label", "--remote-session", "agents"]
            .map(str::to_owned)
            .to_vec();
        let error = parse_add_args(&args).expect_err("test precondition");
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn list_rows_do_not_have_credential_fields() {
        let encoded = serde_json::to_string(&MachineListRow {
            id: "0123456789abcdef0123456789abcdef",
            label: "Build",
            target: "dev@build",
            session: "agents",
            selected: false,
        })
        .expect("test precondition");
        assert!(!encoded.contains("password"));
        assert!(!encoded.contains("key"));
        assert!(!encoded.contains("enabled"));
    }
}
