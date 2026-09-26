use crate::api::schema::{ErrorBody, ErrorResponse, ResponseResult, SuccessResponse};

/// Encodes a success response. A result that fails to encode (serde refuses,
/// for instance, a map with non-string keys) becomes an `internal_error`
/// response for the same request instead of a panic on the main loop.
pub(crate) fn encode_success(id: String, result: ResponseResult) -> String {
    let response = SuccessResponse { id, result };
    serde_json::to_string(&response).unwrap_or_else(|err| {
        encode_error(
            response.id.clone(),
            "internal_error",
            format!("failed to encode response: {err}"),
        )
    })
}

pub(crate) fn encode_error(id: String, code: &str, message: impl Into<String>) -> String {
    encode_error_body(
        id,
        ErrorBody {
            code: code.into(),
            message: message.into(),
        },
    )
}

pub(super) fn encode_error_body(id: String, error: ErrorBody) -> String {
    let response = ErrorResponse { id, error };
    serde_json::to_string(&response).unwrap_or_else(|_| {
        // An error body is plain strings, so this is not expected to happen;
        // `Value`'s `Display` cannot fail, which makes it a safe last resort.
        serde_json::json!({
            "id": response.id,
            "error": {
                "code": response.error.code,
                "message": response.error.message,
            },
        })
        .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_encode_their_id_and_payload() {
        let ok = encode_success("ok".into(), ResponseResult::Ok {});
        let ok: serde_json::Value = serde_json::from_str(&ok).expect("test precondition");
        assert_eq!(ok["id"], "ok");
        assert_eq!(ok["result"]["type"], "ok");

        let error = encode_error("bad".into(), "some_code", "some message");
        let error: ErrorResponse = serde_json::from_str(&error).expect("test precondition");
        assert_eq!(error.id, "bad");
        assert_eq!(error.error.code, "some_code");
        assert_eq!(error.error.message, "some message");
    }
}
