#[derive(Debug, Clone, Copy)]
pub(crate) enum IntegrationActionOutcome {
    Succeeded,
    Failed,
}

pub(crate) fn integration_action(
    action: &'static str,
    target: &'static str,
    outcome: IntegrationActionOutcome,
    error_kind: Option<crate::InstallErrorKind>,
) {
    tracing::info!(
        event = "integration.action",
        subsystem = "integration",
        outcome = ?outcome,
        error_kind = ?error_kind,
        action,
        target,
        "integration action finished"
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
