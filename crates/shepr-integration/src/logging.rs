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
