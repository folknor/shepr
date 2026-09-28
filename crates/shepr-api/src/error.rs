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
    AgentBlocked => "agent_blocked",
    AgentExplainUnavailable => "agent_explain_unavailable",
    AgentNotFound => "agent_not_found",
    AgentNotRunning => "agent_not_running",
    AgentNotReady => "agent_not_ready",
    AgentPromptFailed => "agent_prompt_failed",
    AgentSendKeysFailed => "agent_send_keys_failed",
    CopyMotionUnavailable => "copy_motion_unavailable",
    EmptyAgentPrompt => "empty_agent_prompt",
    InternalError => "internal_error",
    InvalidAgent => "invalid_agent",
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
    AgentLaunchPending => "agent_launch_pending",
    AgentNameTaken => "agent_name_taken",
    AgentNotIdle => "agent_not_idle",
    AgentPaneBusy => "agent_pane_busy",
    AgentPaneNotFound => "agent_pane_not_found",
    AgentPaneUnavailable => "agent_pane_unavailable",
    AgentStartInputFailed => "agent_start_input_failed",
    AgentTargetAmbiguous => "agent_target_ambiguous",
    ClientMissing => "client_missing",
    ConnectionLocalOnly => "connection_local_only",
    EndpointBusy => "endpoint_busy",
    EventsLost => "events_lost",
    InvalidAgentArgument => "invalid_agent_argument",
    InvalidAgentName => "invalid_agent_name",
    InvalidAgentTimeout => "invalid_agent_timeout",
    UnsupportedAgentKind => "unsupported_agent_kind",
    UnsupportedEndpointCommand => "unsupported_endpoint_command",
    UnsupportedMethod => "unsupported_method",
    StaleBoot => "stale_boot",
    SurfaceInactive => "surface_inactive",
    AgentPromptStalled => "agent_prompt_stalled",
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
        ErrorBody {
            code: self.code.as_str().to_owned(),
            message: self.payload.into_message(),
        }
    }
}

pub type ApiResult = Result<ResponseResult, ApiError>;

pub fn encode_result(id: String, result: ApiResult) -> String {
    match result {
        Ok(result) => {
            let response = SuccessResponse { id, result };
            super::serialize_response_or_error(&response.id, &response)
        }
        Err(error) => {
            let response = ErrorResponse {
                id,
                error: error.into_body(),
            };
            super::serialize_response_or_error(&response.id, &response)
        }
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
            ErrorBody {
                code: "pane_not_found".into(),
                message: "pane pane_7 not found".into(),
            }
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
}
