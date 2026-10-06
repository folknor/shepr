pub(crate) fn artifact_installed(integration: &str, artifact: &super::types::InstallArtifact) {
    tracing::info!(
        event = "integration.artifact_installed",
        subsystem = "integration",
        integration,
        role = ?artifact.role,
        path = %artifact.path.display(),
        "integration artifact installed"
    );
}

pub(crate) fn missing_hook_interpreter(target: &'static str) {
    tracing::warn!(
        event = "integration.interpreter_missing",
        subsystem = "integration",
        executable = "python3",
        target,
        "this integration hook requires python3, which was not found on the server PATH; pane PATH may differ, so the hook cannot report from panes where python3 is unavailable"
    );
}
