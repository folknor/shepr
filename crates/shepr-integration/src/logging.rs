pub(crate) fn artifact_installed(integration: &str, artifact: &super::types::InstallArtifact) {
    shepr_platform::structured_log!(
        INFO, event = integration.artifact_install, outcome = Ok,
        integration,
        role = ?artifact.role,
        path = %artifact.path.display(),
        "integration artifact installed"
    );
}

pub(crate) fn missing_hook_interpreter(target: &'static str) {
    shepr_platform::structured_log!(
        WARN,
        event = integration.interpreter,
        outcome = Missing,
        executable = "python3",
        target,
        "this integration hook requires python3, which was not found on the server PATH; pane PATH may differ, so the hook cannot report from panes where python3 is unavailable"
    );
}
