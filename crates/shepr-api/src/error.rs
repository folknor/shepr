use super::schema::{ErrorBody, ErrorResponse, ResponseResult, SuccessResponse};

/// Stable error categories, with one source for enum variants and wire codes.
/// Codes only go out (to the JSON socket or as `EndpointError.code`); nothing
/// parses a wire code back into this enum.
macro_rules! api_error_codes {
    ($($variant:ident => $wire:literal,)+) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum ApiErrorCode {
            $($variant,)+
        }

        impl ApiErrorCode {
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire,)+
                }
            }
        }
    };
}

api_error_codes! {
    AgentExplainFileReadFailed => "agent_explain_file_read_failed",
    AgentExplainUnavailable => "agent_explain_unavailable",
    CopyMotionUnavailable => "copy_motion_unavailable",
    InternalError => "internal_error",
    InvalidAgent => "invalid_agent",
    InvalidCwd => "invalid_cwd",
    InvalidEnv => "invalid_env",
    InvalidPaneSwap => "invalid_pane_swap",
    InvalidRatio => "invalid_ratio",
    InvalidRequest => "invalid_request",
    LayoutNotFound => "layout_not_found",
    PaneClearFailed => "pane_clear_failed",
    PaneLayoutUnavailable => "pane_layout_unavailable",
    PaneNotFound => "pane_not_found",
    PaneSplitFailed => "pane_split_failed",
    QueryTooLarge => "query_too_large",
    SelectionUnavailable => "selection_unavailable",
    SerializationError => "serialization_error",
    ServerUnavailable => "server_unavailable",
    SplitNotFound => "split_not_found",
    Timeout => "timeout",
    WorkspaceCreateFailed => "workspace_create_failed",
    WorkspaceMoveFailed => "workspace_move_failed",
    WorkspaceNotFound => "workspace_not_found",
    EndpointBusy => "endpoint_busy",
    EndpointResponseTooLarge => "endpoint_response_too_large",
    StaleBoot => "stale_boot",
    SurfaceInactive => "surface_inactive",
    BuildMismatch => "build_mismatch",
    ServerNotRunning => "server_not_running",
    ServerStopFailed => "server_stop_failed",
    ServerBootMismatch => "server_boot_mismatch",
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiErrorPayload {
    Message(String),
    PaneNotFound { pane_id: String },
}

impl ApiErrorPayload {
    fn into_message(self) -> String {
        match self {
            Self::Message(message) => message,
            Self::PaneNotFound { pane_id } => format!("pane {pane_id} not found"),
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

    pub fn into_message(self) -> String {
        self.payload.into_message()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_pane_has_a_typed_payload_and_preserves_the_wire_error() {
        let error = ApiError::pane_not_found("w1:p7");
        assert_eq!(error.code, ApiErrorCode::PaneNotFound);
        assert_eq!(
            error.payload,
            ApiErrorPayload::PaneNotFound {
                pane_id: "w1:p7".into()
            },
        );
        assert_eq!(
            error.into_body(),
            ErrorBody::new(&ApiErrorCode::PaneNotFound, "pane w1:p7 not found")
        );
    }
}
