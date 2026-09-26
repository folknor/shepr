use clap::ArgMatches;
use serde::Serialize;

use crate::client::endpoint::{EndpointCatalog, ProfileId};

use super::matches::{flag, required, string};

#[derive(Serialize)]
struct MachineListRow<'a> {
    id: &'a str,
    label: &'a str,
    target: &'a str,
    session: &'a str,
    enabled: bool,
    selected: bool,
}

pub(super) fn run_machine_command(matches: &ArgMatches) -> std::io::Result<i32> {
    match matches.subcommand() {
        Some(("list", matches)) => list(flag(matches, "json")),
        Some(("status", matches)) => {
            status(string(matches, "machine").as_deref(), flag(matches, "json"))
        }
        Some(("reconnect", matches)) => reconnect(&required(matches, "machine")),
        Some(("add", matches)) => add(add_args(matches)),
        Some(("rename", matches)) => rename(
            &required(matches, "profile-id"),
            &required(matches, "label"),
        ),
        Some(("remove", matches)) => remove(&required(matches, "profile-id")),
        Some(("enable", matches)) => set_enabled(&required(matches, "profile-id"), true),
        Some(("disable", matches)) => set_enabled(&required(matches, "profile-id"), false),
        _ => Ok(super::missing_subcommand()),
    }
}

fn list(json: bool) -> std::io::Result<i32> {
    let catalog = load_catalog()?;
    let rows = catalog
        .ssh
        .iter()
        .map(|profile| MachineListRow {
            id: profile.id.as_str(),
            label: &profile.label,
            target: &profile.target,
            session: &profile.session,
            enabled: profile.enabled,
            selected: catalog.selected_profile.as_ref() == Some(&profile.id),
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
        let state = if row.enabled { "enabled" } else { "disabled" };
        println!(
            "{}\t{}\t{}\t{}\t{}",
            row.id, row.label, row.target, row.session, state
        );
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

fn status(selector: Option<&str>, json: bool) -> std::io::Result<i32> {
    let catalog = load_catalog()?;
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
            let (status, error) = if !profile.enabled {
                ("disabled", None)
            } else {
                match crate::remote::check_saved_ssh(&profile.target, &profile.session) {
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

fn reconnect(selector: &str) -> std::io::Result<i32> {
    use std::io::IsTerminal;
    let catalog = load_catalog()?;
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
    let mut authentication = crate::remote::ssh_authentication_command(&profile.target)?;
    if !authentication.command.status()?.success() {
        eprintln!("SSH authentication failed; the saved machine was not changed.");
        return Ok(1);
    }
    crate::remote::check_saved_ssh(&profile.target, &profile.session)?;
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

fn add(args: AddArgs) -> std::io::Result<i32> {
    let AddArgs {
        target,
        label,
        session,
    } = args;
    let mut catalog = load_catalog()?;
    match catalog.add_ssh(label.clone(), &target, session.clone()) {
        Ok(_) => {}
        Err(error) => {
            eprintln!("error: {error}");
            return Ok(2);
        }
    }
    let metadata = match crate::remote::prepare_saved_ssh(&target, &session) {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!("error: {error}; machine was not saved");
            crate::remote::print_saved_ssh_error_hint(&error, &target);
            return Ok(1);
        }
    };
    // Setup can wait for human approval. Do not overwrite catalog edits made meanwhile.
    let mut catalog = load_catalog().map_err(|error| {
        std::io::Error::other(format!(
            "remote prepared, but machine was not saved: {error}"
        ))
    })?;
    let id = match catalog.add_ssh(label, &target, &session) {
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
    if let Some(metadata) = metadata {
        crate::client::endpoint::SshMetadataCache::new(id.as_str(), &target, &session)?
            .store(&metadata);
    }
    println!("Saved SSH machine {id}. Remote server is ready.");
    println!("Open Shepr clients connect automatically.");
    Ok(0)
}

fn rename(raw_id: &str, label: &str) -> std::io::Result<i32> {
    let id = match ProfileId::parse(raw_id.to_owned()) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("error: {error}");
            return Ok(2);
        }
    };
    let mut catalog = load_catalog()?;
    match catalog.rename_ssh(&id, label) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("machine profile {id} was not found");
            return Ok(1);
        }
        Err(error) => {
            eprintln!("error: {error}");
            return Ok(2);
        }
    }
    store_catalog(&catalog)?;
    println!("Renamed SSH machine {id}.");
    Ok(0)
}

fn remove(raw_id: &str) -> std::io::Result<i32> {
    let Some(id) = profile_id(raw_id) else {
        return Ok(2);
    };
    let mut catalog = load_catalog()?;
    let previous_selection = catalog.selected_profile.clone();
    let metadata_cache = catalog
        .ssh
        .iter()
        .find(|profile| profile.id == id)
        .map(|profile| {
            crate::client::endpoint::SshMetadataCache::new(
                id.as_str(),
                &profile.target,
                &profile.session,
            )
        })
        .transpose()?;
    if !catalog.remove_ssh(&id) {
        eprintln!("machine profile {id} was not found");
        return Ok(1);
    }
    store_catalog(&catalog)?;
    if let Some(cache) = metadata_cache {
        cache.invalidate();
    }
    if catalog.selected_profile != previous_selection {
        catalog.store_selection().map_err(std::io::Error::other)?;
    }
    println!("Removed SSH machine {id}.");
    Ok(0)
}

fn set_enabled(raw_id: &str, enabled: bool) -> std::io::Result<i32> {
    let Some(id) = profile_id(raw_id) else {
        return Ok(2);
    };
    let mut catalog = load_catalog()?;
    let previous_selection = catalog.selected_profile.clone();
    if !catalog.set_enabled(&id, enabled) {
        eprintln!("machine profile {id} was not found");
        return Ok(1);
    }
    store_catalog(&catalog)?;
    if catalog.selected_profile != previous_selection {
        catalog.store_selection().map_err(std::io::Error::other)?;
    }
    println!(
        "{} SSH machine {id}.",
        if enabled { "Enabled" } else { "Disabled" }
    );
    Ok(0)
}

fn profile_id(raw: &str) -> Option<ProfileId> {
    match ProfileId::parse(raw.to_owned()) {
        Ok(id) => Some(id),
        Err(error) => {
            eprintln!("error: {error}");
            None
        }
    }
}

fn load_catalog() -> std::io::Result<EndpointCatalog> {
    EndpointCatalog::load().map_err(std::io::Error::other)
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
    fn profile_id_parser_rejects_target_text() {
        assert!(profile_id("build.example").is_none());
    }

    #[test]
    fn list_rows_do_not_have_credential_fields() {
        let encoded = serde_json::to_string(&MachineListRow {
            id: "0123456789abcdef0123456789abcdef",
            label: "Build",
            target: "dev@build",
            session: "agents",
            enabled: true,
            selected: false,
        })
        .expect("test precondition");
        assert!(!encoded.contains("password"));
        assert!(!encoded.contains("key"));
    }
}
