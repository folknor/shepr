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

    if let Some(status) = crate::integration::experimental_letta_integration_status() {
        let state = describe_integration_state(
            status.state,
            status.installed_version,
            status.expected_version,
        );
        println!(
            "{} (experimental): {state} ({})",
            status.label,
            status.path.display()
        );
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

    let installed = match target {
        IntegrationCommandTarget::Builtin(target) => crate::integration::install_target(target),
        IntegrationCommandTarget::Letta => crate::integration::install_experimental_letta(),
    };
    report_outcome(installed)
}

fn integration_uninstall(matches: &ArgMatches) -> i32 {
    let Some(target) = command_target(matches) else {
        return unknown_target(matches);
    };

    let removed = match target {
        IntegrationCommandTarget::Builtin(target) => crate::integration::uninstall_target(target),
        IntegrationCommandTarget::Letta => crate::integration::uninstall_experimental_letta(),
    };
    report_outcome(removed)
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

/// Integration target accepted by the CLI. Letta is not an `IntegrationTarget`
/// variant only for historical reasons: nothing on the wire constrains that
/// enum (client and server are always the same build), so the separate
/// experimental path (this variant, the experimental install/uninstall/status
/// functions in `crate::integration`) is leftover structure that can be folded
/// into `IntegrationTarget` together with its registry and status handling.
/// Letta's install and uninstall already go through the same protected config
/// writer as the built-in targets.
enum IntegrationCommandTarget {
    Builtin(IntegrationTarget),
    Letta,
}

fn command_target(matches: &ArgMatches) -> Option<IntegrationCommandTarget> {
    target_from_label(&matches::required(matches, "target"))
}

/// Maps a `TARGET` value back to its target. The labels are the ones the spec
/// offers as possible values (`integration_target_label` over
/// `IntegrationTarget::ALL`, plus the experimental labels), so there is no
/// second list of names to keep in step.
fn target_from_label(label: &str) -> Option<IntegrationCommandTarget> {
    if label == "letta" {
        return Some(IntegrationCommandTarget::Letta);
    }
    IntegrationTarget::ALL
        .into_iter()
        .find(|target| crate::integration::integration_target_label(*target) == label)
        .map(IntegrationCommandTarget::Builtin)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_labels() -> Vec<&'static str> {
        let mut labels: Vec<&'static str> = IntegrationTarget::ALL
            .into_iter()
            .map(crate::integration::integration_target_label)
            .collect();
        labels.extend_from_slice(crate::integration::EXPERIMENTAL_INTEGRATION_TARGET_LABELS);
        labels
    }

    #[test]
    fn every_target_the_spec_accepts_resolves_to_a_target() {
        for action in ["install", "uninstall"] {
            for label in spec_labels() {
                let matches = crate::cli::tests::command_matches(&["integration", action, label]);
                let target = command_target(&matches)
                    .unwrap_or_else(|| panic!("{action} {label} does not resolve"));
                match target {
                    IntegrationCommandTarget::Builtin(target) => {
                        assert_eq!(crate::integration::integration_target_label(target), label);
                    }
                    IntegrationCommandTarget::Letta => assert_eq!(label, "letta"),
                }
            }
        }
    }

    #[test]
    fn every_experimental_label_has_a_handler() {
        for label in crate::integration::EXPERIMENTAL_INTEGRATION_TARGET_LABELS {
            assert!(
                target_from_label(label).is_some(),
                "experimental label {label} has no handler"
            );
        }
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
