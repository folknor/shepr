//! Codex's `wham/usage` endpoint: rate-limit groups, each with its own
//! verdict and up to two windows, read without guessing which window is
//! which or forcing a verdict into a percentage.

use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::model::{CodexGroup, CodexGroupId, CodexMeasurement, CodexWindow};

pub(crate) const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
pub(crate) const ACCOUNT_HEADER: &str = "ChatGPT-Account-Id";
pub(crate) const FEDRAMP_HEADER: (&str, &str) = ("X-OpenAI-Fedramp", "true");

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn seconds(value: Option<&Value>) -> Option<Duration> {
    value.and_then(Value::as_u64).map(Duration::from_secs)
}

fn window(value: Option<&Value>) -> Option<CodexWindow> {
    let value = value.filter(|value| value.is_object())?;
    Some(CodexWindow {
        used_percent: value
            .get("used_percent")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite()),
        limit_window: seconds(value.get("limit_window_seconds")),
        reset_at: value
            .get("reset_at")
            .and_then(Value::as_i64)
            .and_then(crate::time::from_epoch_seconds),
        reset_after: seconds(value.get("reset_after_seconds")),
    })
}

fn group(id: CodexGroupId, details: &Value) -> CodexGroup {
    CodexGroup {
        id,
        allowed: details.get("allowed").and_then(Value::as_bool),
        limit_reached: details.get("limit_reached").and_then(Value::as_bool),
        primary: window(details.get("primary_window")),
        secondary: window(details.get("secondary_window")),
    }
}

/// Reads a usage response. `None` when the body is not an object, so a
/// changed shape reads as a response failure.
pub(crate) fn parse_usage(body: &Value, observed_at: SystemTime) -> Option<CodexMeasurement> {
    let object = body.as_object()?;
    let mut groups = Vec::new();
    if let Some(details) = object.get("rate_limit").filter(|value| value.is_object()) {
        groups.push(group(CodexGroupId::Main, details));
    }
    if let Some(additional) = object
        .get("additional_rate_limits")
        .and_then(Value::as_array)
    {
        for entry in additional {
            let (Some(limit_name), Some(metered_feature)) =
                (text(entry, "limit_name"), text(entry, "metered_feature"))
            else {
                continue;
            };
            let Some(details) = entry.get("rate_limit").filter(|value| value.is_object()) else {
                continue;
            };
            groups.push(group(
                CodexGroupId::Additional {
                    limit_name,
                    metered_feature,
                },
                details,
            ));
        }
    }
    let plan_type = text(body, "plan_type");
    if groups.is_empty() && plan_type.is_none() {
        return None;
    }
    Some(CodexMeasurement {
        observed_at,
        plan_type,
        groups,
        reached_type: object
            .get("rate_limit_reached_type")
            .and_then(|reached| text(reached, "type")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    #[test]
    fn groups_keep_their_verdicts_and_windows() {
        let body = serde_json::json!({
            "plan_type": "pro",
            "rate_limit": {
                "allowed": true, "limit_reached": false,
                "primary_window": {"used_percent": 12, "limit_window_seconds": 18000,
                                   "reset_after_seconds": 600, "reset_at": 1_791_378_000},
                "secondary_window": null
            },
            "additional_rate_limits": [
                {"limit_name": "codex_other", "metered_feature": "cloud",
                 "rate_limit": {"allowed": false, "limit_reached": true,
                                "primary_window": null, "secondary_window": null}}
            ],
            "rate_limit_reached_type": {"type": "something_new"}
        });
        let measurement = parse_usage(&body, UNIX_EPOCH).expect("usage");
        assert_eq!(measurement.plan_type.as_deref(), Some("pro"));
        assert_eq!(measurement.reached_type.as_deref(), Some("something_new"));
        let main = &measurement.groups[0];
        assert_eq!(main.id, CodexGroupId::Main);
        assert_eq!(main.allowed, Some(true));
        let primary = main.primary.as_ref().expect("primary");
        assert_eq!(primary.used_percent, Some(12.0));
        assert_eq!(primary.limit_window, Some(Duration::from_secs(18_000)));
        assert_eq!(primary.reset_after, Some(Duration::from_secs(600)));
        assert_eq!(
            primary.reset_at,
            Some(UNIX_EPOCH + Duration::from_secs(1_791_378_000))
        );
        assert!(main.secondary.is_none());
        // A verdict survives with no windows to carry it.
        let other = &measurement.groups[1];
        assert_eq!(other.limit_reached, Some(true));
        assert!(other.primary.is_none() && other.secondary.is_none());
    }

    #[test]
    fn missing_fields_stay_unknown() {
        let body = serde_json::json!({"rate_limit": {"primary_window": {}}});
        let measurement = parse_usage(&body, UNIX_EPOCH).expect("usage");
        let main = &measurement.groups[0];
        assert_eq!(main.allowed, None);
        assert_eq!(main.limit_reached, None);
        assert_eq!(main.primary.as_ref().expect("window").used_percent, None);
    }

    #[test]
    fn a_body_with_nothing_recognizable_is_a_failure() {
        assert_eq!(parse_usage(&serde_json::json!({}), UNIX_EPOCH), None);
        assert_eq!(parse_usage(&serde_json::json!("x"), UNIX_EPOCH), None);
    }
}
