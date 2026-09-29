use crate::error::{ApiError, ApiErrorCode};

/// Validates environment entries before they are passed to a pane process.
pub fn validate_launch_env<'a>(
    entries: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<(), ApiError> {
    for (key, value) in entries {
        if key.is_empty() {
            return Err(ApiError::new(
                ApiErrorCode::InvalidEnv,
                format!("env key {key:?} must not be empty"),
            ));
        }
        if key.contains('=') {
            return Err(ApiError::new(
                ApiErrorCode::InvalidEnv,
                format!("env key {key:?} must not contain '='"),
            ));
        }
        if key.contains('\0') {
            return Err(ApiError::new(
                ApiErrorCode::InvalidEnv,
                format!("env key {key:?} must not contain NUL bytes"),
            ));
        }
        if value.contains('\0') {
            return Err(ApiError::new(
                ApiErrorCode::InvalidEnv,
                format!("env value for key {key:?} must not contain NUL bytes"),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_names_the_rejected_key_or_value() {
        for (entry, expected) in [
            (("", "value"), "env key \"\" must not be empty"),
            (
                ("BAD=KEY", "value"),
                "env key \"BAD=KEY\" must not contain '='",
            ),
            (
                ("BAD\0KEY", "value"),
                "env key \"BAD\\0KEY\" must not contain NUL bytes",
            ),
            (
                ("NAME", "BAD\0VALUE"),
                "env value for key \"NAME\" must not contain NUL bytes",
            ),
        ] {
            let error = validate_launch_env([entry]).expect_err("invalid environment entry");
            assert_eq!(error.code, ApiErrorCode::InvalidEnv);
            assert_eq!(error.into_message(), expected);
        }
    }

    #[test]
    fn validation_accepts_empty_values_and_multiple_entries() {
        assert!(validate_launch_env([("ROLE", ""), ("EDITOR", "vim")]).is_ok());
    }
}
