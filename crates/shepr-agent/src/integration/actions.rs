use std::io;

use super::env::AgentIntegrationPaths;
use super::registry::{
    action_label, install_operation, integration_target_label, uninstall_operation,
};
use super::types::{InstallOutcome, UninstallOutcome};
use super::version::{VERSION_PROBE_TIMEOUT, agent_version_requirement, enforce_agent_version};

pub fn install_target(
    paths: &AgentIntegrationPaths,
    target: crate::agent::IntegrationTarget,
) -> io::Result<Vec<String>> {
    let result = install_target_inner(paths, target);
    let outcome = if result.is_ok() { "ok" } else { "error" };
    shepr_platform::logging::integration_action(
        "install",
        integration_target_label(target),
        outcome,
    );
    result
}

fn install_target_inner(
    paths: &AgentIntegrationPaths,
    target: crate::agent::IntegrationTarget,
) -> io::Result<Vec<String>> {
    let version_warning = match agent_version_requirement(target) {
        Some(requirement) => enforce_agent_version(&requirement, VERSION_PROBE_TIMEOUT)?,
        None => None,
    };
    let outcome = install_operation(paths, target)?;
    let mut messages = install_messages(action_label(target), outcome);
    if let Some(warning) = version_warning {
        messages.push(warning);
    }
    Ok(messages)
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
    shepr_platform::logging::integration_action(
        "uninstall",
        integration_target_label(target),
        outcome,
    );
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
