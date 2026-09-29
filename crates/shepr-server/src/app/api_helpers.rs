use shepr_api::error::{ApiError, ApiErrorCode};

pub(crate) fn pane_not_found(pane_id: Option<&str>) -> ApiError {
    match pane_id {
        Some(pane_id) => ApiError::pane_not_found(pane_id),
        None => ApiError::new(ApiErrorCode::PaneNotFound, "active pane not found"),
    }
}

pub(crate) fn pane_in_workspace_not_found(workspace_id: &str) -> ApiError {
    ApiError::new(
        ApiErrorCode::PaneNotFound,
        format!("no pane available in workspace {workspace_id}"),
    )
}

pub(crate) fn workspace_not_found(workspace_id: &str) -> ApiError {
    ApiError::new(
        ApiErrorCode::WorkspaceNotFound,
        format!("workspace {workspace_id} not found"),
    )
}

pub(crate) fn active_workspace_not_found() -> ApiError {
    ApiError::new(ApiErrorCode::WorkspaceNotFound, "no active workspace")
}

pub(crate) fn tab_not_found(tab_id: &str) -> ApiError {
    ApiError::new(ApiErrorCode::TabNotFound, format!("tab {tab_id} not found"))
}

pub(crate) fn tab_for_pane_not_found(pane_id: &str) -> ApiError {
    ApiError::new(
        ApiErrorCode::TabNotFound,
        format!("tab for pane {pane_id} not found"),
    )
}

pub(crate) fn target_pane_not_found(pane_id: &str, tab_id: Option<&str>) -> ApiError {
    let message = match tab_id {
        Some(tab_id) => format!("target pane {pane_id} is not in tab {tab_id}"),
        None => format!("target pane {pane_id} not found"),
    };
    ApiError::new(ApiErrorCode::TargetPaneNotFound, message)
}

pub(super) fn agent_target_not_found(target: &str) -> ApiError {
    ApiError::agent_not_found(target)
}

pub(super) fn detect_state_from_api(
    state: shepr_api::schema::PaneAgentState,
) -> shepr_agent::detect::AgentState {
    match state {
        shepr_api::schema::PaneAgentState::Idle => shepr_agent::detect::AgentState::Idle,
        shepr_api::schema::PaneAgentState::Working => shepr_agent::detect::AgentState::Working,
        shepr_api::schema::PaneAgentState::Blocked => shepr_agent::detect::AgentState::Blocked,
        shepr_api::schema::PaneAgentState::Unknown => shepr_agent::detect::AgentState::Unknown,
    }
}

pub(super) fn pane_agent_status(
    state: shepr_agent::detect::AgentState,
) -> shepr_api::schema::AgentStatus {
    match state.presentation_state() {
        shepr_agent::detect::PresentedAgentState::Idle => shepr_api::schema::AgentStatus::Idle,
        shepr_agent::detect::PresentedAgentState::Working => {
            shepr_api::schema::AgentStatus::Working
        }
        shepr_agent::detect::PresentedAgentState::Blocked => {
            shepr_api::schema::AgentStatus::Blocked
        }
    }
}

/// The format a read produces. `strip_ansi: false` asks to keep escape
/// sequences, which only the ANSI renderer has, so it selects that renderer
/// whatever `format` says; `strip_ansi: true` (the default) leaves `format` in
/// charge. There is no raw PTY byte history to return instead.
pub(super) fn effective_read_format(
    format: shepr_api::schema::ReadFormat,
    strip_ansi: bool,
) -> shepr_api::schema::ReadFormat {
    if strip_ansi {
        format
    } else {
        shepr_api::schema::ReadFormat::Ansi
    }
}

pub(super) fn read_terminal_snapshot(
    terminal: &shepr_mux::pane::PaneRuntime,
    source: shepr_api::schema::ReadSource,
    format: shepr_api::schema::ReadFormat,
    lines: Option<u32>,
) -> Result<shepr_mux::terminal::TerminalReadSnapshot, ApiError> {
    validate_read_request(source, format, lines)?;
    Ok(read_validated_terminal_snapshot(
        terminal, source, format, lines,
    ))
}

fn validate_read_request(
    source: shepr_api::schema::ReadSource,
    format: shepr_api::schema::ReadFormat,
    lines: Option<u32>,
) -> Result<(), ApiError> {
    use shepr_api::schema::{ReadFormat, ReadSource};

    if let Some(lines) = lines
        && lines > MAX_READ_LINES
    {
        return Err(ApiError::new(
            ApiErrorCode::InvalidLines,
            format!("lines must be at most {MAX_READ_LINES}, got {lines}"),
        ));
    }
    if format == ReadFormat::Ansi && source == ReadSource::Detection {
        return Err(ApiError::new(
            ApiErrorCode::UnsupportedReadFormat,
            "the detection source is plain text; read it with format text",
        ));
    }
    Ok(())
}

fn read_validated_terminal_snapshot(
    terminal: &shepr_mux::pane::PaneRuntime,
    source: shepr_api::schema::ReadSource,
    format: shepr_api::schema::ReadFormat,
    lines: Option<u32>,
) -> shepr_mux::terminal::TerminalReadSnapshot {
    use shepr_api::schema::{ReadFormat, ReadSource};

    let line_limit = lines.map(|lines| lines as usize);
    let recent_lines = line_limit.unwrap_or(80);
    match (format, source) {
        (ReadFormat::Text, ReadSource::Visible) => {
            limit_snapshot_lines(terminal.visible_text(), line_limit)
        }
        (ReadFormat::Text, ReadSource::Recent) => terminal.recent_text_snapshot(recent_lines),
        (ReadFormat::Text, ReadSource::RecentUnwrapped) => {
            terminal.recent_unwrapped_text_snapshot(recent_lines)
        }
        // ANSI detection reads are rejected by `validate_read_request`; the
        // arm keeps the match total so it needs no panic arm.
        (ReadFormat::Text | ReadFormat::Ansi, ReadSource::Detection) => {
            limit_snapshot_lines(terminal.detection_text(), line_limit)
        }
        (ReadFormat::Ansi, ReadSource::Visible) => {
            limit_snapshot_lines(terminal.visible_ansi(), line_limit)
        }
        (ReadFormat::Ansi, ReadSource::Recent) => terminal.recent_ansi_snapshot(recent_lines),
        (ReadFormat::Ansi, ReadSource::RecentUnwrapped) => {
            terminal.recent_unwrapped_ansi_snapshot(recent_lines)
        }
    }
}

pub(crate) fn limit_snapshot_lines(
    text: String,
    limit: Option<usize>,
) -> shepr_mux::terminal::TerminalReadSnapshot {
    let Some(limit) = limit else {
        return shepr_mux::terminal::TerminalReadSnapshot {
            text,
            truncated: false,
        };
    };
    let lines: Vec<_> = text.split_inclusive('\n').collect();
    shepr_mux::terminal::TerminalReadSnapshot {
        text: lines[lines.len().saturating_sub(limit)..].concat(),
        truncated: lines.len() > limit,
    }
}

pub(super) fn normalize_reported_agent_label(agent: &str) -> Option<String> {
    let trimmed = agent.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(agent) = shepr_agent::detect::parse_agent_label(trimmed) {
        return Some(shepr_agent::detect::agent_label(agent).to_string());
    }
    Some(trimmed.to_string())
}

pub(super) use crate::limits::MAX_READ_LINES;

#[cfg(test)]
mod agent_status_tests {
    use super::pane_agent_status;
    use shepr_agent::detect::AgentState;
    use shepr_api::schema::AgentStatus;

    #[test]
    fn unknown_agent_state_presents_as_idle() {
        assert_eq!(pane_agent_status(AgentState::Unknown), AgentStatus::Idle);
    }
}

#[cfg(test)]
mod read_snapshot_tests {
    use super::{
        MAX_READ_LINES, effective_read_format, limit_snapshot_lines, validate_read_request,
    };
    use shepr_api::schema::{ReadFormat, ReadSource};

    #[test]
    fn keeping_escapes_selects_the_ansi_renderer() {
        assert_eq!(
            effective_read_format(ReadFormat::Text, true),
            ReadFormat::Text
        );
        assert_eq!(
            effective_read_format(ReadFormat::Ansi, true),
            ReadFormat::Ansi
        );
        assert_eq!(
            effective_read_format(ReadFormat::Text, false),
            ReadFormat::Ansi
        );
        assert_eq!(
            effective_read_format(ReadFormat::Ansi, false),
            ReadFormat::Ansi
        );
    }

    #[test]
    fn oversized_line_requests_are_rejected_instead_of_capped() {
        assert!(
            validate_read_request(ReadSource::Recent, ReadFormat::Text, Some(MAX_READ_LINES))
                .is_ok()
        );
        let error = validate_read_request(
            ReadSource::Recent,
            ReadFormat::Text,
            Some(MAX_READ_LINES + 1),
        )
        .expect_err("test precondition");
        assert_eq!(error.code, shepr_api::error::ApiErrorCode::InvalidLines);
    }

    #[test]
    fn ansi_detection_reads_are_rejected_instead_of_returning_plain_text() {
        let error = validate_read_request(ReadSource::Detection, ReadFormat::Ansi, None)
            .expect_err("test precondition");
        assert_eq!(
            error.code,
            shepr_api::error::ApiErrorCode::UnsupportedReadFormat
        );
        assert!(validate_read_request(ReadSource::Detection, ReadFormat::Text, None).is_ok());
        assert!(validate_read_request(ReadSource::Visible, ReadFormat::Ansi, None).is_ok());
    }

    #[test]
    fn line_limit_preserves_endings_and_reports_omitted_lines() {
        let snapshot = limit_snapshot_lines("one\ntwø\n三\n".into(), Some(2));
        assert_eq!(snapshot.text, "twø\n三\n");
        assert!(snapshot.truncated);

        let snapshot = limit_snapshot_lines("one\ntwo\nthree".into(), Some(1));
        assert_eq!(snapshot.text, "three");
        assert!(snapshot.truncated);

        let snapshot = limit_snapshot_lines("one\ntwo".into(), Some(0));
        assert_eq!(snapshot.text, "");
        assert!(snapshot.truncated);

        let snapshot = limit_snapshot_lines(String::new(), Some(2));
        assert_eq!(snapshot.text, "");
        assert!(!snapshot.truncated);
    }

    #[test]
    fn omitted_line_limit_returns_the_complete_snapshot() {
        let snapshot = limit_snapshot_lines("one\ntwo\n".into(), None);
        assert_eq!(snapshot.text, "one\ntwo\n");
        assert!(!snapshot.truncated);
    }
}

#[cfg(test)]
mod not_found_tests {
    use super::{
        active_workspace_not_found, agent_target_not_found, pane_in_workspace_not_found,
        pane_not_found, tab_for_pane_not_found, tab_not_found, target_pane_not_found,
        workspace_not_found,
    };
    use shepr_api::error::ApiErrorCode;

    #[test]
    fn not_found_helpers_keep_the_subject_and_code_together() {
        let error = pane_not_found(Some("w1:p2"));
        assert_eq!(error.code, ApiErrorCode::PaneNotFound);
        assert_eq!(error.into_message(), "pane w1:p2 not found");

        let error = pane_not_found(None);
        assert_eq!(error.code, ApiErrorCode::PaneNotFound);
        assert_eq!(error.into_message(), "active pane not found");

        let error = pane_in_workspace_not_found("w1");
        assert_eq!(error.code, ApiErrorCode::PaneNotFound);
        assert_eq!(error.into_message(), "no pane available in workspace w1");

        let error = workspace_not_found("w9");
        assert_eq!(error.code, ApiErrorCode::WorkspaceNotFound);
        assert_eq!(error.into_message(), "workspace w9 not found");

        let error = active_workspace_not_found();
        assert_eq!(error.code, ApiErrorCode::WorkspaceNotFound);
        assert_eq!(error.into_message(), "no active workspace");

        let error = tab_not_found("w1:t4");
        assert_eq!(error.code, ApiErrorCode::TabNotFound);
        assert_eq!(error.into_message(), "tab w1:t4 not found");

        let error = tab_for_pane_not_found("w1:p2");
        assert_eq!(error.code, ApiErrorCode::TabNotFound);
        assert_eq!(error.into_message(), "tab for pane w1:p2 not found");

        let error = target_pane_not_found("w1:p2", None);
        assert_eq!(error.code, ApiErrorCode::TargetPaneNotFound);
        assert_eq!(error.into_message(), "target pane w1:p2 not found");

        let error = target_pane_not_found("w1:p2", Some("w1:t4"));
        assert_eq!(error.code, ApiErrorCode::TargetPaneNotFound);
        assert_eq!(
            error.into_message(),
            "target pane w1:p2 is not in tab w1:t4"
        );

        let error = agent_target_not_found("reviewer");
        assert_eq!(error.code, ApiErrorCode::AgentNotFound);
        assert_eq!(error.into_message(), "agent target reviewer not found");
    }
}
