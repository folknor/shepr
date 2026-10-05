use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;

use shepr_agent::IntegrationTarget;

use super::config_file::is_config_changed;
use super::env::AgentIntegrationPaths;
use super::registry::{
    action_label, agent_present, install_operation, integration_status, managed_assets,
};
use super::types::{InstallError, InstallOutcome, InstallOutput, IntegrationStatusKind};

/// Installs or updates shepr's hooks for every supported agent present on
/// this host. The server caller decides whether this release-only operation
/// should run because agent configs are shared across build profiles.
///
/// An agent is present when its own config directory already exists; an
/// absent agent is skipped and its directory is never created. A target
/// whose installed integration is already current is left untouched, so a
/// launch that finds nothing to do writes nothing. Everything is reported
/// through `tracing`: a target that cannot be checked or installed is logged
/// and the others still run, and nothing here fails the caller.
///
/// This does file IO, so a server calls it off its startup path.
pub fn install_present_integrations(paths: &AgentIntegrationPaths) {
    let mut python3_available = None;
    for target in IntegrationTarget::all() {
        let label = target.label();
        match install_if_present(paths, target, &mut python3_available) {
            Ok(Some(output)) => {
                for message in output.messages {
                    tracing::info!(integration = label, "{message}");
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    integration = label,
                    error_kind = ?error.kind(),
                    %error,
                    "could not check or install the agent integration"
                );
            }
        }
    }
}

/// The install output when `target`'s agent is present and its integration
/// was missing or outdated, `None` when there was nothing to do.
fn install_if_present(
    paths: &AgentIntegrationPaths,
    target: IntegrationTarget,
    python3_available: &mut Option<bool>,
) -> Result<Option<InstallOutput>, InstallError> {
    let present = match agent_present(paths, target).map_err(InstallError::from) {
        Ok(present) => present,
        Err(error) => {
            crate::logging::integration_action(
                "status",
                target.label(),
                crate::logging::IntegrationActionOutcome::Failed,
                Some(error.kind()),
            );
            return Err(error);
        }
    };
    if !present {
        tracing::debug!(
            integration = target.label(),
            "agent not present; integration skipped"
        );
        return Ok(None);
    }
    if managed_assets(target).any(|asset| asset.contents.contains("command -v python3")) {
        let python3_available =
            *python3_available.get_or_insert_with(python3_available_on_server_path);
        if !python3_available {
            crate::logging::missing_hook_interpreter(target.label());
        }
    }
    let status = match integration_status(paths, target) {
        Ok(status) => status,
        Err(error) => {
            crate::logging::integration_action(
                "status",
                target.label(),
                crate::logging::IntegrationActionOutcome::Failed,
                Some(error.kind()),
            );
            return Err(error);
        }
    };
    crate::logging::integration_action(
        "status",
        target.label(),
        crate::logging::IntegrationActionOutcome::Succeeded,
        None,
    );
    if status.state == IntegrationStatusKind::Current {
        tracing::debug!(
            integration = target.label(),
            path = %status.path.display(),
            "integration is current"
        );
        return Ok(None);
    }
    tracing::info!(
        integration = status.target.label(),
        path = %status.path.display(),
        state = ?status.state,
        outdated_reason = ?status.outdated_reason,
        installed_version = ?status.installed_version,
        "installing the agent integration"
    );
    install_target(paths, target).map(Some)
}

/// Whether `python3` resolves on the server's own `PATH`. A pane's `PATH` can
/// differ, so this only decides whether to warn.
fn python3_available_on_server_path() -> bool {
    // A refused read cannot happen for a raw variable; treat it as unset.
    let path = shepr_core::env::read_os(shepr_core::env::EnvVar::Path)
        .ok()
        .flatten();
    python3_available_on_path(path.as_deref())
}

fn python3_available_on_path(path: Option<&OsStr>) -> bool {
    let Some(path) = path else {
        return false;
    };
    std::env::split_paths(path).any(|directory| {
        fs::metadata(directory.join("python3"))
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    })
}

pub(crate) fn install_target(
    paths: &AgentIntegrationPaths,
    target: IntegrationTarget,
) -> Result<InstallOutput, InstallError> {
    let result = install_target_inner(paths, target).map_err(InstallError::from);
    let (outcome, error_kind) = match &result {
        Ok(_) => (crate::logging::IntegrationActionOutcome::Succeeded, None),
        Err(error) => (
            crate::logging::IntegrationActionOutcome::Failed,
            Some(error.kind()),
        ),
    };
    crate::logging::integration_action("install", target.label(), outcome, error_kind);
    result
}

fn install_target_inner(
    paths: &AgentIntegrationPaths,
    target: IntegrationTarget,
) -> io::Result<InstallOutput> {
    // Agent processes do not honor Shepr's config lock. If an agent changes a
    // config after the install read it, reload the config and retry once.
    let outcome = match install_operation(paths, target) {
        Err(error) if is_config_changed(&error) => install_operation(paths, target)?,
        result => result?,
    };
    Ok(InstallOutput {
        messages: install_messages(action_label(target), outcome),
    })
}

fn install_messages(label: &str, outcome: InstallOutcome) -> Vec<String> {
    let mut messages = outcome
        .artifacts
        .iter()
        .map(|artifact| artifact.role.install_message(label, &artifact.path))
        .collect::<Vec<_>>();
    messages.extend(outcome.notices);
    messages
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::python3_available_on_path;

    #[test]
    fn python3_probe_uses_the_supplied_path() {
        let scratch = shepr_test_support::ScratchDir::new("hook-python-path");
        let bin = scratch.path().join("bin");
        let empty = scratch.path().join("empty");
        fs::create_dir_all(&bin).expect("test precondition");
        fs::create_dir_all(&empty).expect("test precondition");
        let python3 = bin.join("python3");
        fs::write(&python3, "").expect("test precondition");
        fs::set_permissions(&python3, fs::Permissions::from_mode(0o755))
            .expect("test precondition");

        assert!(python3_available_on_path(Some(bin.as_os_str())));
        assert!(!python3_available_on_path(Some(empty.as_os_str())));
    }
}
