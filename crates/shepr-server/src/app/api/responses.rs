use shepr_api::error::{ApiError, ApiErrorCode};

/// A failed JSON API method. Client-shell commands answer with
/// `EndpointError` instead (`endpoint.rs`).
pub(crate) fn failure<T>(
    code: impl Into<ApiErrorCode>,
    message: impl Into<String>,
) -> Result<T, ApiError> {
    Err(ApiError::new(code.into(), message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_api::schema::ResponseResult;

    #[test]
    fn failure_preserves_typed_error() {
        assert_eq!(
            failure::<ResponseResult>(ApiErrorCode::InvalidRequest, "bad request"),
            Err(ApiError::new(ApiErrorCode::InvalidRequest, "bad request")),
        );
    }
}
