use clap::ArgMatches;

use crate::api::schema::IntegrationTarget;

use super::matches;

/// `shepr integration <install|uninstall|status>`. The clap spec
/// (`integration_command` in `spec.rs`) has already required a subcommand and
/// restricted `TARGET` to the known labels, so usage errors and help never
/// reach this function.
pub(super) fn run_integration_command(matches: &ArgMatches) -> std::io::Result<i32> {
    match matches.subcommand() {
        Some(("install", matches)) => Ok(integration_install(matches)),
        Some(("uninstall", matches)) => Ok(integration_uninstall(matches)),
        Some(("status", matches)) => {
            Ok(integration_status(matches::flag(matches, "outdated-only")))
        }
        _ => Ok(super::missing_subcommand()),
    }
}

fn integration_status(outdated_only: bool) -> i32 {
    if outdated_only {
        crate::integration::print_outdated_update_notice();
        return 0;
    }

    for status in crate::integration::installed_integration_statuses() {
        let target = crate::integration::integration_target_label(status.target);
        let state = describe_integration_state(
            status.state,
            status.installed_version,
            status.expected_version,
        );
        println!("{target}: {state} ({})", status.path.display());
    }

    0
}

fn describe_integration_state(
    state: crate::integration::IntegrationStatusKind,
    installed_version: Option<u32>,
    expected_version: u32,
) -> String {
    let version = match installed_version {
        Some(version) => format!("v{version}"),
        None => "legacy".to_string(),
    };
    match state {
        crate::integration::IntegrationStatusKind::NotInstalled => "not installed".to_string(),
        crate::integration::IntegrationStatusKind::Current => format!("current ({version})"),
        crate::integration::IntegrationStatusKind::Outdated
            if installed_version.is_some_and(|installed| installed >= expected_version) =>
        {
            format!("needs repair ({version})")
        }
        crate::integration::IntegrationStatusKind::Outdated => {
            format!("outdated ({version} < v{expected_version})")
        }
    }
}

fn integration_install(matches: &ArgMatches) -> i32 {
    let Some(target) = command_target(matches) else {
        return unknown_target(matches);
    };

    report_outcome(crate::integration::install_target(target))
}

fn integration_uninstall(matches: &ArgMatches) -> i32 {
    let Some(target) = command_target(matches) else {
        return unknown_target(matches);
    };

    report_outcome(crate::integration::uninstall_target(target))
}

fn report_outcome(outcome: std::io::Result<Vec<String>>) -> i32 {
    match outcome {
        Ok(messages) => {
            for message in messages {
                println!("{message}");
            }
            0
        }
        Err(err) => {
            eprintln!("{err}");
            1
        }
    }
}

/// Only reachable if the spec's possible values and [`target_from_label`]
/// disagree; the test below keeps them in step.
fn unknown_target(matches: &ArgMatches) -> i32 {
    super::usage_error(&format!(
        "unknown integration target: {}",
        matches::required(matches, "target")
    ))
}

fn command_target(matches: &ArgMatches) -> Option<IntegrationTarget> {
    target_from_label(&matches::required(matches, "target"))
}

/// Maps a `TARGET` value back to its target. The labels are the ones the spec
/// offers as possible values (`integration_target_label` over
/// `IntegrationTarget::ALL`), so there is no second list of names to keep in
/// step.
fn target_from_label(label: &str) -> Option<IntegrationTarget> {
    IntegrationTarget::ALL
        .into_iter()
        .find(|target| crate::integration::integration_target_label(*target) == label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_target_the_spec_accepts_resolves_to_a_target() {
        for action in ["install", "uninstall"] {
            for target in IntegrationTarget::ALL {
                let label = crate::integration::integration_target_label(target);
                let matches = crate::cli::tests::command_matches(&["integration", action, label]);
                assert_eq!(
                    command_target(&matches),
                    Some(target),
                    "{action} {label} does not resolve"
                );
            }
        }
    }

    #[test]
    fn letta_is_an_ordinary_target() {
        assert_eq!(target_from_label("letta"), Some(IntegrationTarget::Letta));
    }

    #[test]
    fn unknown_labels_do_not_resolve() {
        assert!(target_from_label("nope").is_none());
        assert!(target_from_label("").is_none());
        // Only the hyphenated label is a target name.
        assert!(target_from_label("antigravity_cli").is_none());
    }

    #[test]
    fn status_reads_the_outdated_only_flag() {
        let status = crate::cli::tests::command_matches(&["integration", "status"]);
        assert!(!matches::flag(&status, "outdated-only"));
        let status =
            crate::cli::tests::command_matches(&["integration", "status", "--outdated-only"]);
        assert!(matches::flag(&status, "outdated-only"));
    }
}
