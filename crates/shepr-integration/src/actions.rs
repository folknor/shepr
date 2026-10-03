use std::io;

use shepr_agent::IntegrationTarget;

use super::config_file::is_config_changed;
use super::env::AgentIntegrationPaths;
use super::registry::{action_label, agent_present, install_operation, integration_status};
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
    for target in IntegrationTarget::all() {
        let label = target.label();
        match install_if_present(paths, target) {
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
