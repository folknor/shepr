use shepr_api::schema::IntegrationTarget;

use super::matches;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Install { target: String },
    Uninstall { target: String },
    Status { outdated_only: bool },
}

impl Command {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::Install { .. } => "install",
            Self::Uninstall { .. } => "uninstall",
            Self::Status { .. } => "status",
        }
    }

    pub(super) fn can_run_on_machine(&self) -> bool {
        match self {
            Self::Install { .. } | Self::Uninstall { .. } | Self::Status { .. } => false,
        }
    }
}

pub(super) fn parse(matches: &clap::ArgMatches) -> Option<Command> {
    match matches.subcommand() {
        Some(("install", command)) => Some(Command::Install {
            target: matches::required(command, "target")?,
        }),
        Some(("uninstall", command)) => Some(Command::Uninstall {
            target: matches::required(command, "target")?,
        }),
        Some(("status", command)) => Some(Command::Status {
            outdated_only: matches::flag(command, "outdated-only"),
        }),
        _ => None,
    }
}

/// `shepr integration <install|uninstall|status>`. The clap spec
/// (`integration_command` in `spec.rs`) has already required a subcommand and
/// restricted `TARGET` to the known labels, so usage errors and help never
/// reach this function.
pub(super) fn run_integration_command(
    command: Command,
    _paths: &shepr_config::AppPaths,
) -> super::CliResult<i32> {
    let integration_paths = shepr_agent::integration::AgentIntegrationPaths::resolve();
    match command {
        Command::Install { target } => integration_install(&target, &integration_paths),
        Command::Uninstall { target } => integration_uninstall(&target, &integration_paths),
        Command::Status { outdated_only } => {
            Ok(integration_status(&integration_paths, outdated_only))
        }
    }
}

fn integration_status(
    paths: &shepr_agent::integration::AgentIntegrationPaths,
    outdated_only: bool,
) -> i32 {
    if outdated_only {
        // A notice, not a failure: it goes to stderr and the check still
        // exits 0.
        if let Some(notice) = shepr_agent::integration::outdated_update_notice(paths) {
            shepr_platform::begin_cli_output();
            eprintln!("{notice}");
        }
        return 0;
    }

    let mut unresolved = false;
    for row in shepr_agent::integration::integration_status_rows(paths) {
        match row {
            Ok(status) => {
                let target = shepr_agent::integration::integration_target_label(status.target);
                let state = describe_integration_state(
                    status.state,
                    status.installed_version,
                    status.expected_version,
                );
                println!("{target}: {state} ({})", status.path.display());
            }
            Err(error) => {
                // The message names what could not be checked (an unresolved
                // directory or a failed stat), so it is shown as given.
                unresolved = true;
                let target = shepr_agent::integration::integration_target_label(error.target);
                println!("{target}: unknown ({})", error.message);
            }
        }
    }

    // A target that could not be checked is a failed check, not a clean report.
    i32::from(unresolved)
}

fn describe_integration_state(
    state: shepr_agent::integration::IntegrationStatusKind,
    installed_version: Option<u32>,
    expected_version: u32,
) -> String {
    let version = match installed_version {
        Some(version) => format!("v{version}"),
        None => "legacy".to_string(),
    };
    match state {
        shepr_agent::integration::IntegrationStatusKind::NotInstalled => {
            "not installed".to_string()
        }
        shepr_agent::integration::IntegrationStatusKind::Current => format!("current ({version})"),
        shepr_agent::integration::IntegrationStatusKind::Outdated
            if installed_version.is_some_and(|installed| installed >= expected_version) =>
        {
            format!("needs repair ({version})")
        }
        shepr_agent::integration::IntegrationStatusKind::Outdated => {
            format!("outdated ({version} < v{expected_version})")
        }
    }
}

fn integration_install(
    label: &str,
    paths: &shepr_agent::integration::AgentIntegrationPaths,
) -> super::CliResult<i32> {
    let target = target_from_label(label).ok_or_else(|| unknown_target(label))?;
    report_install_outcome(shepr_agent::integration::install_target(paths, target))
}

fn integration_uninstall(
    label: &str,
    paths: &shepr_agent::integration::AgentIntegrationPaths,
) -> super::CliResult<i32> {
    let target = target_from_label(label).ok_or_else(|| unknown_target(label))?;
    report_outcome(shepr_agent::integration::uninstall_target(paths, target))
}

/// Prints what an install or uninstall did. A failure travels to the CLI's
/// error printer like every other command's.
fn report_outcome(outcome: std::io::Result<Vec<String>>) -> super::CliResult<i32> {
    for message in outcome? {
        println!("{message}");
    }
    Ok(0)
}

fn report_install_outcome(
    outcome: std::io::Result<shepr_agent::integration::InstallOutput>,
) -> super::CliResult<i32> {
    let output = outcome?;
    for message in output.messages {
        println!("{message}");
    }
    for warning in output.warnings {
        super::print_notice(&format!("warning: {warning}"));
    }
    Ok(0)
}

/// Only reachable if the spec's possible values and [`target_from_label`]
/// disagree; the test below keeps them in step.
fn unknown_target(target: &str) -> super::CliError {
    super::CliError::Usage(format!("unknown integration target: {target}"))
}

/// Maps a `TARGET` value back to its target. The labels are the ones the spec
/// offers as possible values (`integration_target_label` over
/// `IntegrationTarget::all()`), so there is no second list of names to keep in
/// step.
fn target_from_label(label: &str) -> Option<IntegrationTarget> {
    IntegrationTarget::all()
        .find(|target| shepr_agent::integration::integration_target_label(*target) == label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_target_the_spec_accepts_resolves_to_a_target() {
        for action in ["install", "uninstall"] {
            for target in IntegrationTarget::all() {
                let label = shepr_agent::integration::integration_target_label(target);
                assert_eq!(
                    target_from_label(label),
                    Some(target),
                    "{action} {label} does not resolve"
                );
            }
        }
    }

    #[test]
    fn unknown_labels_do_not_resolve() {
        assert!(target_from_label("nope").is_none());
        // Detected from the screen only; no hook integration exists.
        for label in ["letta", "qodercli", "qwen"] {
            assert!(target_from_label(label).is_none(), "{label}");
        }
        assert!(target_from_label("").is_none());
        // Process aliases are not integration command labels.
        assert!(target_from_label("antigravity-cli").is_none());
        assert!(target_from_label("antigravity_cli").is_none());
    }

    #[test]
    fn status_reads_the_outdated_only_flag() {
        let status = crate::cli::tests::group_matches(&["integration", "status"]);
        assert_eq!(
            super::parse(&status).expect("test precondition"),
            Command::Status {
                outdated_only: false
            }
        );
        let status =
            crate::cli::tests::group_matches(&["integration", "status", "--outdated-only"]);
        assert_eq!(
            super::parse(&status).expect("test precondition"),
            Command::Status {
                outdated_only: true
            }
        );
    }
}
