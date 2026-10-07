//! Claude's OAuth usage and profile endpoints: what is asked and how the
//! answers are read. `limits[]` is the source of truth; the legacy
//! `five_hour` and `seven_day` blocks fill only the windows it lacks.

use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::model::{ClaudeLimit, ClaudeMeasurement, Plan};

pub(crate) const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub(crate) const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
pub(crate) const BETA_HEADER: (&str, &str) = ("anthropic-beta", "oauth-2025-04-20");

const SESSION_KIND: &str = "session";
const WEEKLY_KIND: &str = "weekly_all";
const LEGACY_FIVE_HOUR: &str = "five_hour";
const LEGACY_SEVEN_DAY: &str = "seven_day";
// limits-exempt: the window lengths the session and weekly kinds name.
const FIVE_HOURS: Duration = Duration::from_secs(5 * 60 * 60);
// limits-exempt: as above.
const SEVEN_DAYS: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The identity and plan `/profile` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Profile {
    pub(crate) account_uuid: Option<String>,
    pub(crate) organization_uuid: Option<String>,
    pub(crate) email: Option<String>,
    pub(crate) plan: Plan,
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

pub(crate) fn parse_profile(body: &Value) -> Option<Profile> {
    let account = body.get("account")?;
    let organization = body.get("organization");
    let org_text = |key| organization.and_then(|organization| text(organization, key));
    Some(Profile {
        account_uuid: text(account, "uuid"),
        organization_uuid: org_text("uuid"),
        email: text(account, "email").or_else(|| text(account, "email_address")),
        plan: Plan {
            kind: org_text("organization_type"),
            tier: org_text("rate_limit_tier"),
            status: org_text("subscription_status"),
        },
    })
}

/// A percent that is a finite number; anything else is unknown, never zero.
fn percent(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
}

fn reset(value: Option<&Value>) -> Option<SystemTime> {
    match value? {
        Value::String(text) => crate::time::parse_rfc3339(text),
        Value::Number(number) => number.as_i64().and_then(crate::time::from_epoch_seconds),
        _ => None,
    }
}

/// Reads a usage response. `None` when the body is not an object with any
/// recognizable limits, so a changed shape reads as a response failure
/// rather than an empty measurement.
pub(crate) fn parse_usage(body: &Value, observed_at: SystemTime) -> Option<ClaudeMeasurement> {
    let object = body.as_object()?;
    let mut limits = Vec::new();
    if let Some(entries) = object.get("limits").and_then(Value::as_array) {
        for entry in entries {
            let Some(kind) = text(entry, "kind") else {
                continue;
            };
            let scope = entry.get("scope");
            let model = scope
                .and_then(|scope| scope.get("model"))
                .and_then(|model| text(model, "display_name").or_else(|| text(model, "id")));
            let surface = scope.and_then(|scope| text(scope, "surface"));
            let duration = match kind.as_str() {
                SESSION_KIND => Some(FIVE_HOURS),
                WEEKLY_KIND | "weekly_scoped" => Some(SEVEN_DAYS),
                _ => None,
            };
            limits.push(ClaudeLimit {
                used_percent: percent(entry.get("percent")),
                resets_at: reset(entry.get("resets_at")),
                is_active: entry.get("is_active").and_then(Value::as_bool),
                kind,
                model,
                surface,
                duration,
                legacy: false,
            });
        }
    }
    let has = |limits: &[ClaudeLimit], kind: &str| {
        limits
            .iter()
            .any(|limit| !limit.legacy && limit.kind == kind && limit.model.is_none())
    };
    for (legacy, modern, duration) in [
        (LEGACY_FIVE_HOUR, SESSION_KIND, FIVE_HOURS),
        (LEGACY_SEVEN_DAY, WEEKLY_KIND, SEVEN_DAYS),
    ] {
        let Some(block) = object.get(legacy).filter(|block| block.is_object()) else {
            continue;
        };
        if has(&limits, modern) {
            continue;
        }
        limits.push(ClaudeLimit {
            kind: legacy.to_owned(),
            model: None,
            surface: None,
            duration: Some(duration),
            used_percent: percent(block.get("utilization")),
            resets_at: reset(block.get("resets_at")),
            is_active: None,
            legacy: true,
        });
    }
    // A recognized shape with no windows is a real, empty measurement: it
    // withdraws whatever the last one showed. Only a body with neither
    // `limits` nor a legacy block is unrecognizable.
    let recognized = object.get("limits").is_some_and(Value::is_array)
        || [LEGACY_FIVE_HOUR, LEGACY_SEVEN_DAY]
            .iter()
            .any(|key| object.get(*key).is_some_and(Value::is_object));
    if !recognized {
        return None;
    }
    Some(ClaudeMeasurement {
        observed_at,
        limits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn limits_come_first_and_legacy_fills_only_what_is_missing() {
        let body = serde_json::json!({
            "limits": [
                {"kind": "session", "percent": 42.5, "resets_at": "2026-10-07T13:00:00Z", "is_active": true},
                {"kind": "weekly_scoped", "percent": 10, "scope": {"model": {"display_name": "Opus"}}}
            ],
            "five_hour": {"utilization": 99, "resets_at": "2026-10-07T14:00:00Z"},
            "seven_day": {"utilization": 30, "resets_at": "2026-10-10T00:00:00Z"}
        });
        let measurement = parse_usage(&body, at(5)).expect("limits");
        assert_eq!(measurement.observed_at, at(5));
        assert_eq!(measurement.limits.len(), 3);
        let session = &measurement.limits[0];
        assert_eq!(session.kind, "session");
        assert_eq!(session.used_percent, Some(42.5));
        assert_eq!(session.resets_at, Some(at(1_791_378_000)));
        assert_eq!(session.duration, Some(FIVE_HOURS));
        assert_eq!(session.is_active, Some(true));
        let scoped = &measurement.limits[1];
        assert_eq!(scoped.model.as_deref(), Some("Opus"));
        // The five-hour legacy block is ignored, the weekly one fills the gap.
        let weekly = &measurement.limits[2];
        assert_eq!(weekly.kind, "seven_day");
        assert!(weekly.legacy);
        assert_eq!(weekly.used_percent, Some(30.0));
    }

    #[test]
    fn a_missing_percent_stays_unknown() {
        let body = serde_json::json!({"limits": [{"kind": "session", "percent": null}]});
        let measurement = parse_usage(&body, at(0)).expect("limits");
        assert_eq!(measurement.limits[0].used_percent, None);
        assert_eq!(measurement.limits[0].resets_at, None);
    }

    #[test]
    fn an_unrecognizable_body_is_not_an_empty_measurement() {
        assert_eq!(parse_usage(&serde_json::json!({"other": 1}), at(0)), None);
        assert_eq!(parse_usage(&serde_json::json!([1, 2]), at(0)), None);
    }

    #[test]
    fn an_empty_limits_list_is_a_real_empty_measurement() {
        let measurement =
            parse_usage(&serde_json::json!({"limits": []}), at(3)).expect("recognized shape");
        assert!(measurement.limits.is_empty());
        assert_eq!(measurement.observed_at, at(3));
    }

    #[test]
    fn profile_reads_identity_and_plan() {
        let body = serde_json::json!({
            "account": {"uuid": "acc", "email": "a@example.com"},
            "organization": {"uuid": "org", "organization_type": "claude_max",
                             "rate_limit_tier": "default_claude_max_5x", "subscription_status": "active"}
        });
        let profile = parse_profile(&body).expect("profile");
        assert_eq!(profile.account_uuid.as_deref(), Some("acc"));
        assert_eq!(profile.organization_uuid.as_deref(), Some("org"));
        assert_eq!(profile.email.as_deref(), Some("a@example.com"));
        assert_eq!(profile.plan.kind.as_deref(), Some("claude_max"));
        assert_eq!(profile.plan.tier.as_deref(), Some("default_claude_max_5x"));
    }
}
