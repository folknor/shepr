use super::schema::{ErrorBody, ErrorResponse, ResponseResult, SuccessResponse};

/// Stable error categories, with one source for enum variants and wire codes.
macro_rules! api_error_codes {
    ($($variant:ident => $wire:literal,)+) => {
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum ApiErrorCode {
            $($variant,)+
            /// A code introduced by a newer or different server build.
            Unknown(String),
        }

        impl ApiErrorCode {
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $wire,)+
                    Self::Unknown(value) => value,
                }
            }

            fn from_wire(value: String) -> Self {
                match value.as_str() {
                    $($wire => Self::$variant,)+
                    _ => Self::Unknown(value),
                }
            }
        }

        impl serde::Serialize for ApiErrorCode {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> serde::Deserialize<'de> for ApiErrorCode {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = <String as serde::Deserialize>::deserialize(deserializer)?;
                Ok(Self::from_wire(value))
            }
        }
    };
}

api_error_codes! {
    AgentExplainUnavailable => "agent_explain_unavailable",
    InvalidAgent => "invalid_agent",
    InvalidRequest => "invalid_request",
    InvalidPaneId => "invalid_pane_id",
    PaneNotFound => "pane_not_found",
    PaneTerminalUnavailable => "pane_terminal_unavailable",
    SerializationError => "serialization_error",
    ServerUnavailable => "server_unavailable",
    Timeout => "timeout",
    EndpointBusy => "endpoint_busy",
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
    ClientDisconnected,
}

impl ApiLogOutcome {
    // Emit the log schema string only after request handling has kept the outcome typed.
    pub(crate) const fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Timeout => "timeout",
            Self::Error => "error",
            Self::ClientDisconnected => "client_disconnected",
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
                id: Some(id),
                error: error.into_body(),
            };
            super::serialize_response_or_error_with_outcome(
                response.id.as_deref().unwrap_or_default(),
                &response,
            )
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

    #[test]
    fn unknown_wire_error_codes_are_preserved() {
        let code: ApiErrorCode =
            serde_json::from_str("\"new_server_error\"").expect("string error codes decode");
        assert_eq!(code, ApiErrorCode::Unknown("new_server_error".into()));
        assert_eq!(
            serde_json::to_string(&code).expect("error codes encode as strings"),
            "\"new_server_error\""
        );
    }
}
