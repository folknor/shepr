use crate::api::error::{ApiError, ApiErrorCode, ApiResult};
use crate::api::schema::ResponseResult;

pub(crate) fn success(_id: String, result: ResponseResult) -> ApiResult {
    Ok(result)
}

pub(crate) fn failure(
    _id: String,
    code: impl Into<ApiErrorCode>,
    message: impl Into<String>,
) -> ApiResult {
    Err(ApiError::new(code.into(), message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_builders_keep_typed_payloads() {
        assert_eq!(
            success("ok".into(), ResponseResult::Ok {}),
            Ok(ResponseResult::Ok {})
        );
        assert_eq!(
            failure("bad".into(), ApiErrorCode::InvalidRequest, "bad request"),
            Err(ApiError::new(ApiErrorCode::InvalidRequest, "bad request")),
        );
    }
}
