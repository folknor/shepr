use std::collections::HashMap;

use shepr_api::error::ApiError;

pub(super) fn normalize_launch_env(
    env: HashMap<String, String>,
) -> Result<Vec<(String, String)>, ApiError> {
    // Keep all pane launch environment checks in the API validator so every
    // creation route returns the same error code and wording.
    shepr_api::launch_env::validate_launch_env(
        env.iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
    )?;
    let mut normalized: Vec<_> = env.into_iter().collect();
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
                .into_message(),
            "env key \"BAD=KEY\" must not contain '='"
        );
    }
}
