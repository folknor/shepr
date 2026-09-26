use clap::ArgMatches;
use serde::Serialize;

use crate::client::endpoint::EndpointCatalog;

use super::matches::{flag, required, string};

#[derive(Serialize)]
struct MachineListRow<'a> {
    id: &'a str,
    label: &'a str,
    target: &'a str,
    session: &'a str,
    selected: bool,
}

pub(super) fn run_machine_command(
    matches: &ArgMatches,
    context: &super::target::CliContext,
) -> std::io::Result<i32> {
    let paths: &crate::config::AppPaths = context;
    match matches.subcommand() {
        Some(("list", matches)) => list(paths, flag(matches, "json")),
        Some(("status", matches)) => status(
            string(matches, "machine").as_deref(),
            flag(matches, "json"),
            paths,
            saved_ssh_settings(paths)?,
        ),
        Some(("reconnect", matches)) => reconnect(
            paths,
            &required(matches, "machine"),
            saved_ssh_settings(paths)?,
        ),
        Some(("add", matches)) => add(paths, add_args(matches), saved_ssh_settings(paths)?),
        Some(("remove", matches)) => remove(paths, &required(matches, "machine")),
        _ => Ok(super::missing_subcommand()),
    }
}

fn list(paths: &crate::config::AppPaths, json: bool) -> std::io::Result<i32> {
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
    paths: &crate::config::AppPaths,
    settings: crate::remote::SavedSshSettings,
) -> std::io::Result<i32> {
    let catalog = load_catalog(paths)?;
    let profiles = match selector {
        Some(selector) => match super::target::resolve_machine(&catalog.ssh, selector) {
            Ok(profile) => vec![profile],
            Err(error) => {
                eprintln!("{error}");
                return Ok(2);
            }
        },
        None => catalog.ssh.iter().collect(),
    };
    let rows = profiles
        .into_iter()
        .map(|profile| {
            let (status, error) = match crate::remote::check_saved_ssh(
                paths,
                &profile.target,
                &profile.session,
                settings,
            ) {
                Ok(()) => ("reachable", None),
                Err(error) => {
                    let message = error.to_string();
                    let status = if crate::remote::ssh_error_requires_authentication(&message) {
                        "auth required"
                    } else {
                        "error"
                    };
                    (status, Some(message))
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
    paths: &crate::config::AppPaths,
    selector: &str,
    settings: crate::remote::SavedSshSettings,
) -> std::io::Result<i32> {
    use std::io::IsTerminal;
    let catalog = load_catalog(paths)?;
    let profile = match super::target::resolve_machine(&catalog.ssh, selector) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("{error}");
            return Ok(2);
        }
    };
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "reconnect requires an interactive terminal; use shepr machine status for noninteractive checks"
        );
        return Ok(2);
    }
    let mut authentication =
        crate::remote::ssh_authentication_command(paths, &profile.target, settings)?;
    if !authentication.command.status()?.success() {
        eprintln!("SSH authentication failed; the saved machine was not changed.");
        return Ok(1);
    }
    crate::remote::check_saved_ssh(paths, &profile.target, &profile.session, settings)?;
    println!(
        "Machine {} is reachable. Open Shepr clients retry within 30 seconds.",
        profile.id
    );
    Ok(0)
}

#[derive(Debug, PartialEq, Eq)]
struct AddArgs {
    target: String,
    label: String,
    session: String,
}

fn add_args(matches: &ArgMatches) -> AddArgs {
    AddArgs {
        target: required(matches, "ssh-target"),
        label: required(matches, "label"),
        session: string(matches, "remote-session")
            .unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_owned()),
    }
}

fn add(
    paths: &crate::config::AppPaths,
    args: AddArgs,
    settings: crate::remote::SavedSshSettings,
) -> std::io::Result<i32> {
    let AddArgs {
        target,
        label,
        session,
    } = args;
    let target = match crate::remote::SshTarget::parse(target) {
        Ok(target) => target,
        Err(error) => {
            eprintln!("error: {error}");
            return Ok(2);
        }
    };
    let mut catalog = load_catalog(paths)?;
    // This preflight validates fields and capacity before remote setup can wait. Its ID is
    // intentionally discarded; IDs identify saved rows. Duplicate labels are permitted,
    // and selectors report ambiguity so callers can use the profile ID.
    match catalog.add_ssh(label.clone(), target.clone(), session.clone()) {
        Ok(_) => {}
        Err(error) => {
            eprintln!("error: {error}");
            return Ok(2);
        }
    }
    let executable = match crate::remote::prepare_saved_ssh(paths, &target, &session, settings) {
        Ok(executable) => executable,
        Err(error) => {
            eprintln!("error: {error}; machine was not saved");
            crate::remote::print_saved_ssh_error_hint(&error, &target);
            return Ok(1);
        }
    };
    // Setup can wait for human approval. Do not overwrite catalog edits made meanwhile.
    let mut catalog = load_catalog(paths).map_err(|error| {
        std::io::Error::other(format!(
            "remote prepared, but machine was not saved: {error}"
        ))
    })?;
    let id = match catalog.add_ssh(label, target.clone(), &session) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("error: {error}");
            return Ok(2);
        }
    };
    store_catalog(&catalog).map_err(|error| {
        std::io::Error::other(format!(
            "remote prepared, but machine was not saved: {error}"
        ))
    })?;
    crate::client::endpoint::SshMetadataCache::new(paths, &id, &target, &session)
        .store(&executable);
    println!("Saved SSH machine {id}. Remote server is ready.");
    println!("Open Shepr clients connect automatically.");
    Ok(0)
}

fn saved_ssh_settings(
    paths: &crate::config::AppPaths,
) -> std::io::Result<crate::remote::SavedSshSettings> {
    let config = super::load_validated_config(paths)?;
    Ok(crate::remote::SavedSshSettings {
        manage_ssh_config: config.remote.manage_ssh_config,
    })
}

fn remove(paths: &crate::config::AppPaths, selector: &str) -> std::io::Result<i32> {
    let mut catalog = load_catalog(paths)?;
    let profile = match super::target::resolve_machine(&catalog.ssh, selector) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("{error}");
            return Ok(2);
        }
    };
    let id = profile.id.clone();
    let was_selected = catalog.load_selection().as_ref() == Some(&id);
    let metadata_cache = crate::client::endpoint::SshMetadataCache::new(
        paths,
        &id,
        profile.target.as_str(),
        &profile.session,
    );
    if !catalog.remove_ssh(&id) {
        eprintln!("machine profile {id} was not found");
        return Ok(1);
    }
    store_catalog(&catalog)?;
    metadata_cache.invalidate();
    // The next launch falls back to Local instead of naming a removed machine.
    if was_selected {
        catalog
            .store_selection(None)
            .map_err(std::io::Error::other)?;
    }
    println!("Removed SSH machine {id}.");
    Ok(0)
}

fn load_catalog(paths: &crate::config::AppPaths) -> std::io::Result<EndpointCatalog> {
    EndpointCatalog::load(paths).map_err(std::io::Error::other)
}

fn store_catalog(catalog: &EndpointCatalog) -> std::io::Result<()> {
    catalog.store_profiles().map_err(std::io::Error::other)
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
