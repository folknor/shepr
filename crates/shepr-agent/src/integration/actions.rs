use std::io;

use crate::limits::VERSION_PROBE_TIMEOUT;

use super::env::AgentIntegrationPaths;
use super::registry::{
    action_label, install_operation, integration_target_label, uninstall_operation,
};
use super::types::{InstallOutcome, InstallOutput, UninstallOutcome};
use super::version::{agent_version_requirement, enforce_agent_version};

pub fn install_target(
    paths: &AgentIntegrationPaths,
    target: crate::agent::IntegrationTarget,
) -> io::Result<InstallOutput> {
    let result = install_target_inner(paths, target);
    let outcome = if result.is_ok() { "ok" } else { "error" };
    crate::logging::integration_action("install", integration_target_label(target), outcome);
    result
}

fn install_target_inner(
    paths: &AgentIntegrationPaths,
    target: crate::agent::IntegrationTarget,
) -> io::Result<InstallOutput> {
    let version_warning = match agent_version_requirement(target) {
        Some(requirement) => enforce_agent_version(&requirement, VERSION_PROBE_TIMEOUT)?,
        None => None,
    };
    let outcome = install_operation(paths, target)?;
    Ok(InstallOutput {
        messages: install_messages(action_label(target), outcome),
        warnings: version_warning.into_iter().collect(),
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

pub fn uninstall_target(
    paths: &AgentIntegrationPaths,
    target: crate::agent::IntegrationTarget,
) -> io::Result<Vec<String>> {
    let result = uninstall_target_inner(paths, target);
    let outcome = if result.is_ok() { "ok" } else { "error" };
    crate::logging::integration_action("uninstall", integration_target_label(target), outcome);
    result
}

fn uninstall_target_inner(
    paths: &AgentIntegrationPaths,
    target: crate::agent::IntegrationTarget,
) -> io::Result<Vec<String>> {
    let outcome = uninstall_operation(paths, target)?;
    Ok(uninstall_messages(action_label(target), &outcome))
}

fn uninstall_messages(label: &str, outcome: &UninstallOutcome) -> Vec<String> {
    outcome
        .artifacts
        .iter()
        .filter_map(|artifact| {
            artifact
                .role
                .uninstall_message(label, &artifact.path, artifact.state)
        })
        .collect()
}
