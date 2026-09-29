use super::schema::{ErrorBody, ErrorResponse, ResponseResult, SuccessResponse};

/// Stable error categories, with one source for enum variants and wire codes.
macro_rules! api_error_codes {
    ($($variant:ident => $wire:literal,)+) => {
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum ApiErrorCode {
            $($variant,)+
            /// Keeps a code from an error producer outside this typed path.
            External(String),
        }

        impl ApiErrorCode {
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $wire,)+
                    Self::External(code) => code,
                }
            }
        }

        impl From<&str> for ApiErrorCode {
            fn from(code: &str) -> Self {
                match code {
                    $($wire => Self::$variant,)+
                    other => Self::External(other.to_owned()),
                }
            }
        }
    };
}

api_error_codes! {
    AgentExplainFileReadFailed => "agent_explain_file_read_failed",
    AgentExplainUnavailable => "agent_explain_unavailable",
    AgentNotFound => "agent_not_found",
    CopyMotionUnavailable => "copy_motion_unavailable",
    InternalError => "internal_error",
    InvalidAgent => "invalid_agent",
    InvalidCwd => "invalid_cwd",
    InvalidEnv => "invalid_env",
    InvalidKey => "invalid_key",
    InvalidLayout => "invalid_layout",
    InvalidLines => "invalid_lines",
    InvalidMetadataRequest => "invalid_metadata_request",
    InvalidMetadataSource => "invalid_metadata_source",
    InvalidMetadataToken => "invalid_metadata_token",
    InvalidMetadataTtl => "invalid_metadata_ttl",
    InvalidPaneSwap => "invalid_pane_swap",
    InvalidParams => "invalid_params",
    InvalidRatio => "invalid_ratio",
    InvalidRegex => "invalid_regex",
    InvalidRequest => "invalid_request",
    InvalidSshAgent => "invalid_ssh_agent",
    InvalidTarget => "invalid_target",
    LayoutApplyFailed => "layout_apply_failed",
    LayoutNotFound => "layout_not_found",
    MetadataSequenceSourceLimit => "metadata_sequence_source_limit",
    MetadataTokenLimit => "metadata_token_limit",
    PaneClearFailed => "pane_clear_failed",
    PaneClosed => "pane_closed",
    PaneLayoutUnavailable => "pane_layout_unavailable",
    PaneMoveFailed => "pane_move_failed",
    PaneNotFound => "pane_not_found",
    PaneSendFailed => "pane_send_failed",
    PaneSplitFailed => "pane_split_failed",
    QueryTooLarge => "query_too_large",
    SelectionUnavailable => "selection_unavailable",
    SerializationError => "serialization_error",
    ServerUnavailable => "server_unavailable",
    SplitNotFound => "split_not_found",
    SshAgentUnavailable => "ssh_agent_unavailable",
    StaleContent => "stale_content",
    TabCloseFailed => "tab_close_failed",
    TabCreateFailed => "tab_create_failed",
    TabMoveFailed => "tab_move_failed",
    TabNotFound => "tab_not_found",
    TargetPaneNotFound => "target_pane_not_found",
    Timeout => "timeout",
    UnsupportedEventWaitMatch => "unsupported_event_wait_match",
    UnsupportedReadFormat => "unsupported_read_format",
    WorkspaceCreateFailed => "workspace_create_failed",
    WorkspaceMoveBlockFailed => "workspace_move_block_failed",
    WorkspaceMoveFailed => "workspace_move_failed",
    WorkspaceNotFound => "workspace_not_found",
    AgentNameTaken => "agent_name_taken",
    AgentNotIdle => "agent_not_idle",
    AgentTargetAmbiguous => "agent_target_ambiguous",
    ClientMissing => "client_missing",
    ConnectionLocalOnly => "connection_local_only",
    EndpointBusy => "endpoint_busy",
    EventsLost => "events_lost",
    InvalidAgentName => "invalid_agent_name",
    UnsupportedEndpointCommand => "unsupported_endpoint_command",
    UnsupportedMethod => "unsupported_method",
    StaleBoot => "stale_boot",
    SurfaceInactive => "surface_inactive",
    BuildMismatch => "build_mismatch",
    InvalidSessionName => "invalid_session_name",
    ServerNotRunning => "server_not_running",
    SessionDeleteFailed => "session_delete_failed",
    SessionStopFailed => "session_stop_failed",
}

impl From<&String> for ApiErrorCode {
    fn from(code: &String) -> Self {
        Self::from(code.as_str())
    }
}

impl From<String> for ApiErrorCode {
    fn from(code: String) -> Self {
        Self::from(code.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiErrorPayload {
    Message(String),
    PaneNotFound { pane_id: String },
    AgentNotFound { target: String },
}

impl ApiErrorPayload {
    fn into_message(self) -> String {
        match self {
            Self::Message(message) => message,
            Self::PaneNotFound { pane_id } => format!("pane {pane_id} not found"),
            Self::AgentNotFound { target } => format!("agent target {target} not found"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub code: ApiErrorCode,
    pub payload: ApiErrorPayload,
}

impl ApiError {
    pub fn new(code: ApiErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            payload: ApiErrorPayload::Message(message.into()),
        }
    }

    pub fn pane_not_found(pane_id: impl Into<String>) -> Self {
        Self {
            code: ApiErrorCode::PaneNotFound,
            payload: ApiErrorPayload::PaneNotFound {
                pane_id: pane_id.into(),
            },
        }
    }

    pub fn agent_not_found(target: impl Into<String>) -> Self {
        Self {
            code: ApiErrorCode::AgentNotFound,
            payload: ApiErrorPayload::AgentNotFound {
                target: target.into(),
            },
        }
    }

    pub fn into_message(self) -> String {
        self.payload.into_message()
    }

    pub fn from_body(body: ErrorBody) -> Self {
        Self::new(ApiErrorCode::from(body.code.as_str()), body.message)
    }

    pub fn into_body(self) -> ErrorBody {
        ErrorBody::new(&self.code, self.payload.into_message())
    }
}

pub type ApiResult = Result<ResponseResult, ApiError>;

pub fn encode_result(id: String, result: ApiResult) -> String {
    encode_result_with_outcome(id, result).body
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApiLogOutcome {
    Ok,
    Timeout,
    Error,
}

impl ApiLogOutcome {
    pub(crate) const fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Timeout => "timeout",
            Self::Error => "error",
        }
    }

    fn for_error_code(code: &str) -> Self {
        if code == ApiErrorCode::Timeout.as_str() {
            Self::Timeout
        } else {
            Self::Error
        }
    }
}

#[derive(Debug)]
pub(crate) struct EncodedApiResponse {
    pub(crate) body: String,
    pub(crate) outcome: ApiLogOutcome,
}

pub(crate) fn encode_result_with_outcome(id: String, result: ApiResult) -> EncodedApiResponse {
    let outcome = match &result {
        Ok(_) => ApiLogOutcome::Ok,
        Err(error) if error.code == ApiErrorCode::Timeout => ApiLogOutcome::Timeout,
        Err(_) => ApiLogOutcome::Error,
    };
    let response = match result {
        Ok(result) => {
            let response = SuccessResponse { id, result };
            super::serialize_response_or_error_with_outcome(&response.id, &response)
        }
        Err(error) => {
            let response = ErrorResponse {
                id,
                error: error.into_body(),
            };
            super::serialize_response_or_error_with_outcome(&response.id, &response)
        }
    };
    EncodedApiResponse {
        body: response.body,
        outcome: if response.outcome == ApiLogOutcome::Error {
            ApiLogOutcome::Error
        } else {
            outcome
        },
    }
}

pub(crate) fn encode_error_response_with_outcome(response: &ErrorResponse) -> EncodedApiResponse {
    let outcome = ApiLogOutcome::for_error_code(response.error.code.as_str());
    let encoded = super::serialize_response_or_error_with_outcome(&response.id, &response);
    EncodedApiResponse {
        body: encoded.body,
        outcome: if encoded.outcome == ApiLogOutcome::Error {
            ApiLogOutcome::Error
        } else {
            outcome
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_pane_has_a_typed_payload_and_preserves_the_wire_error() {
        let error = ApiError::pane_not_found("pane_7");
        assert_eq!(error.code, ApiErrorCode::PaneNotFound);
        assert_eq!(
            error.payload,
            ApiErrorPayload::PaneNotFound {
                pane_id: "pane_7".into()
            },
        );
        assert_eq!(
            error.into_body(),
            ErrorBody::new(&ApiErrorCode::PaneNotFound, "pane pane_7 not found")
        );
    }

    #[test]
    fn legacy_wire_codes_are_classified_before_internal_use() {
        let error = ApiError::from_body(ErrorBody {
            code: "agent_target_ambiguous".into(),
            message: "more than one agent matches".into(),
        });
        assert_eq!(error.code, ApiErrorCode::AgentTargetAmbiguous);
    }

    #[test]
    fn prebuilt_error_responses_log_timeout_only_for_the_timeout_code() {
        let response = |code: &ApiErrorCode| ErrorResponse {
            id: "req".into(),
            error: ErrorBody::new(code, "message"),
        };
        let timeout = encode_error_response_with_outcome(&response(&ApiErrorCode::Timeout));
        assert_eq!(timeout.outcome, ApiLogOutcome::Timeout);
        assert_eq!(timeout.outcome.as_str(), "timeout");
        let parsed: ErrorResponse = serde_json::from_str(&timeout.body).expect("test precondition");
        assert_eq!(parsed.error.code, ApiErrorCode::Timeout.as_str());

        let other = encode_error_response_with_outcome(&response(&ApiErrorCode::PaneNotFound));
        assert_eq!(other.outcome, ApiLogOutcome::Error);
        assert_eq!(other.outcome.as_str(), "error");
        assert_eq!(ApiLogOutcome::Ok.as_str(), "ok");
    }
}
