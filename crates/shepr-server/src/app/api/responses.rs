use shepr_api::error::{ApiError, ApiErrorCode, ApiResult};
use shepr_api::schema::ResponseResult;

pub(crate) fn success(result: ResponseResult) -> ApiResult {
    Ok(result)
}

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

    #[test]
    fn response_builders_keep_typed_payloads() {
        assert_eq!(success(ResponseResult::Ok {}), Ok(ResponseResult::Ok {}));
        assert_eq!(
            failure::<ResponseResult>(ApiErrorCode::InvalidRequest, "bad request"),
            Err(ApiError::new(ApiErrorCode::InvalidRequest, "bad request")),
        );
    }
}
