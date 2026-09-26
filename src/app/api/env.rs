use std::collections::HashMap;

use crate::api::error::{ApiError, ApiErrorCode};

pub(super) fn normalize_launch_env(
    env: HashMap<String, String>,
) -> Result<Vec<(String, String)>, ApiError> {
    let mut normalized = Vec::with_capacity(env.len());
    for (key, value) in env {
        if key.is_empty() {
            return Err(ApiError::new(
                ApiErrorCode::InvalidEnv,
                "env key must not be empty",
            ));
        }
        if key.contains('=') {
            return Err(ApiError::new(
                ApiErrorCode::InvalidEnv,
                format!("env key {key} must not contain '='"),
            ));
        }
        if key.contains('\0') {
            return Err(ApiError::new(
                ApiErrorCode::InvalidEnv,
                "env key must not contain NUL bytes",
            ));
        }
        if value.contains('\0') {
            return Err(ApiError::new(
                ApiErrorCode::InvalidEnv,
                format!("env value for {key} must not contain NUL bytes"),
            ));
        }
        normalized.push((key, value));
    }
    normalized.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_launch_env_sorts_entries() {
        let env = HashMap::from([
            ("ZED".to_string(), "last".to_string()),
            ("ALPHA".to_string(), "first".to_string()),
        ]);

        assert_eq!(
            normalize_launch_env(env).expect("test precondition"),
            vec![
                ("ALPHA".to_string(), "first".to_string()),
                ("ZED".to_string(), "last".to_string()),
            ]
        );
    }

    #[test]
    fn normalize_launch_env_rejects_invalid_keys() {
        let env = HashMap::from([("BAD=KEY".to_string(), "value".to_string())]);

        assert_eq!(
            normalize_launch_env(env)
                .expect_err("test precondition")
                .code,
            ApiErrorCode::InvalidEnv
        );
    }
}
