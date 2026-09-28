use shepr_api::error::{ApiError, ApiErrorCode, ApiResult};
use shepr_api::schema::ResponseResult;

pub(crate) fn success(result: ResponseResult) -> ApiResult {
    Ok(result)
}

pub(crate) fn failure(code: impl Into<ApiErrorCode>, message: impl Into<String>) -> ApiResult {
    Err(ApiError::new(code.into(), message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_builders_keep_typed_payloads() {
        assert_eq!(success(ResponseResult::Ok {}), Ok(ResponseResult::Ok {}));
        assert_eq!(
            failure(ApiErrorCode::InvalidRequest, "bad request"),
            Err(ApiError::new(ApiErrorCode::InvalidRequest, "bad request")),
        );
    }
}
