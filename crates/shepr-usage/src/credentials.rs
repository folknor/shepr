//! Agent credential files: Claude Code's `.credentials.json` and Codex's
//! `auth.json`, read for the access token and the facts that route and bound
//! it. Refresh tokens are never deserialized. Nothing here writes a file or
//! refreshes a token.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::secret::Secret;
use crate::source::Provider;

/// A credential generation: a versioned, tagged digest of the fields that
/// decide routing and authentication. Two reads with the same digest are the
/// same credential as far as a request is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Generation([u8; 32]);

impl Generation {
    /// A short, non-reversible label for logs and diagnostics.
    pub fn short(self) -> String {
        self.0[..4]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// The usable result of parsing a credential file.
#[derive(Debug, Clone)]
pub(crate) struct Credential {
    pub(crate) provider: Provider,
    pub(crate) generation: Generation,
    pub(crate) access_token: Secret,
    /// When the access token stops being accepted, if known.
    pub(crate) expires_at: Option<SystemTime>,
    pub(crate) routing: Routing,
    pub(crate) identity: IdentityHints,
}

/// What a request needs beyond the bearer token.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Routing {
    /// Codex: the `ChatGPT-Account-Id` header value.
    pub(crate) account_id: Option<String>,
    /// Codex: route through the FedRAMP backend.
    pub(crate) fedramp: bool,
}

/// Identity evidence from the file itself. For Codex these are unverified
/// claims; for Claude identity comes from `/profile` instead.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct IdentityHints {
    pub(crate) email: Option<String>,
    /// Codex: the principal the access token's claims name.
    pub(crate) principal: Option<String>,
    /// Codex: whether the id token names the same principal as the access
    /// token. `None` when either side is missing.
    pub(crate) id_token_bound: Option<bool>,
    pub(crate) plan: Option<String>,
}

/// What a credential file says when it is read and well formed.
#[derive(Debug, Clone)]
pub(crate) enum ParsedCredentials {
    Usable(Credential),
    /// Signed out: no OAuth tokens in the file.
    LoggedOut,
    /// An API-key login, which has no subscription limits.
    ApiKey,
    /// The file is well formed JSON but not a shape this reads.
    Unsupported,
}

/// Why a credential file could not be parsed. A parse failure may be a
/// write in progress (Codex writes in place), so it is reported, not taken as
/// a logout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParseFailure {
    NotJson,
}

/// Claude Code's `.credentials.json`, typed so only these fields are ever
/// materialized: every other field (the refresh token among them) is skipped
/// by the parser without being allocated.
#[derive(serde::Deserialize)]
struct ClaudeFile {
    #[serde(rename = "claudeAiOauth", default)]
    oauth: Option<ClaudeOauth>,
}

#[derive(serde::Deserialize)]
struct ClaudeOauth {
    #[serde(rename = "accessToken", default)]
    access_token: Option<String>,
    #[serde(rename = "expiresAt", default)]
    expires_at: Option<Value>,
    #[serde(default)]
    scopes: Option<Value>,
    #[serde(rename = "subscriptionType", default)]
    subscription_type: Option<Value>,
}

/// Codex's `auth.json`, typed the same way: the refresh token is skipped
/// unallocated, and the API key's value is never read, only its presence.
#[derive(serde::Deserialize)]
struct CodexFile {
    #[serde(default)]
    auth_mode: Option<Value>,
    #[serde(rename = "OPENAI_API_KEY", default)]
    api_key: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    tokens: Option<CodexTokens>,
}

#[derive(serde::Deserialize)]
struct CodexTokens {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    account_id: Option<Value>,
    #[serde(default)]
    chatgpt_account_is_fedramp: Option<Value>,
}

pub(crate) fn parse(provider: Provider, bytes: &[u8]) -> Result<ParsedCredentials, ParseFailure> {
    // A parse error carries text from the file; only its kind is kept. The
    // first pass checks the JSON is well formed without building anything,
    // so a torn write is told apart from a shape this does not read.
    serde_json::from_slice::<serde::de::IgnoredAny>(bytes).map_err(|_| ParseFailure::NotJson)?;
    Ok(match provider {
        Provider::Claude => match serde_json::from_slice::<ClaudeFile>(bytes) {
            Ok(file) => parse_claude(file),
            Err(_) => ParsedCredentials::Unsupported,
        },
        Provider::Codex => match serde_json::from_slice::<CodexFile>(bytes) {
            Ok(file) => parse_codex(file),
            Err(_) => ParsedCredentials::Unsupported,
        },
    })
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn scalar_text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// A token string, empty meaning absent, held as a [`Secret`] at once.
fn token(value: Option<String>) -> Option<Secret> {
    value
        .filter(|token| !token.is_empty())
        .map(|token| Secret::new(token.into_bytes()))
}

fn parse_claude(file: ClaudeFile) -> ParsedCredentials {
    let Some(oauth) = file.oauth else {
        return ParsedCredentials::LoggedOut;
    };
    let Some(access_token) = token(oauth.access_token) else {
        return ParsedCredentials::LoggedOut;
    };
    let expires_at = oauth
        .expires_at
        .as_ref()
        .and_then(Value::as_u64)
        .and_then(|millis| UNIX_EPOCH.checked_add(Duration::from_millis(millis)));
    let scopes: Vec<&str> = oauth
        .scopes
        .as_ref()
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |scopes| {
            scopes.iter().filter_map(Value::as_str).collect()
        });
    // A token minted without the profile scope (`claude setup-token`) cannot
    // read usage; it is not a credential this tracker can use.
    if !scopes.is_empty() && !scopes.contains(&"user:profile") {
        return ParsedCredentials::Unsupported;
    }
    let routing = Routing::default();
    let generation = generation(Provider::Claude, access_token.expose(), &routing, "oauth");
    ParsedCredentials::Usable(Credential {
        provider: Provider::Claude,
        generation,
        access_token,
        expires_at,
        routing,
        identity: IdentityHints {
            plan: scalar_text(oauth.subscription_type.as_ref()),
            ..IdentityHints::default()
        },
    })
}

const OPENAI_AUTH_CLAIM: &str = "https://api.openai.com/auth";
const OPENAI_PROFILE_CLAIM: &str = "https://api.openai.com/profile";

fn parse_codex(file: CodexFile) -> ParsedCredentials {
    let auth_mode = scalar_text(file.auth_mode.as_ref());
    let (access_token, id_token, account_id, fedramp) = match file.tokens {
        Some(tokens) => (
            token(tokens.access_token),
            token(tokens.id_token),
            scalar_text(tokens.account_id.as_ref()),
            tokens
                .chatgpt_account_is_fedramp
                .as_ref()
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
        None => (None, None, None, false),
    };
    let Some(access_token) = access_token else {
        return if file.api_key.is_some() || auth_mode.as_deref() == Some("apikey") {
            ParsedCredentials::ApiKey
        } else {
            ParsedCredentials::LoggedOut
        };
    };
    if auth_mode
        .as_deref()
        .is_some_and(|mode| mode.eq_ignore_ascii_case("apikey"))
    {
        return ParsedCredentials::ApiKey;
    }
    let access_claims = jwt_claims(access_token.expose());
    let id_claims = id_token
        .as_ref()
        .and_then(|token| jwt_claims(token.expose()));
    let auth = |claims: &Option<Value>, key: &str| {
        claims
            .as_ref()
            .and_then(|claims| claims.get(OPENAI_AUTH_CLAIM))
            .and_then(|auth| text(auth, key))
    };
    let account_id = account_id.or_else(|| auth(&access_claims, "chatgpt_account_id"));
    let principal_of = |claims: &Option<Value>| {
        auth(claims, "chatgpt_user_id")
            .or_else(|| auth(claims, "user_id"))
            .or_else(|| claims.as_ref().and_then(|claims| text(claims, "sub")))
    };
    let principal = principal_of(&access_claims);
    let id_principal = principal_of(&id_claims);
    let id_token_bound = match (&principal, &id_principal) {
        (Some(access), Some(id)) => Some(access == id),
        _ => None,
    };
    let email = id_claims.as_ref().and_then(|claims| {
        claims
            .get(OPENAI_PROFILE_CLAIM)
            .and_then(|profile| text(profile, "email"))
            .or_else(|| text(claims, "email"))
    });
    let expires_at = access_claims
        .as_ref()
        .and_then(|claims| claims.get("exp"))
        .and_then(Value::as_i64)
        .and_then(crate::time::from_epoch_seconds);
    let plan =
        auth(&access_claims, "chatgpt_plan_type").or_else(|| auth(&id_claims, "chatgpt_plan_type"));
    let routing = Routing {
        account_id,
        fedramp,
    };
    let generation = generation(
        Provider::Codex,
        access_token.expose(),
        &routing,
        auth_mode.as_deref().unwrap_or(""),
    );
    ParsedCredentials::Usable(Credential {
        provider: Provider::Codex,
        generation,
        access_token,
        expires_at,
        routing,
        identity: IdentityHints {
            email,
            principal,
            id_token_bound,
            plan,
        },
    })
}

/// The unverified payload of a JWT. Claims are evidence from the credential
/// file, not proof of anything.
fn jwt_claims(token: &[u8]) -> Option<Value> {
    let token = std::str::from_utf8(token).ok()?;
    let mut parts = token.split('.');
    let (_header, payload) = (parts.next()?, parts.next()?);
    parts.next()?;
    let payload = payload.trim_end_matches('=');
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The versioned, tagged generation digest: each field is tagged and length
/// prefixed, so no two different credentials share an encoding.
fn generation(provider: Provider, token: &[u8], routing: &Routing, auth_mode: &str) -> Generation {
    let mut hasher = Sha256::new();
    let mut field = |tag: &[u8], value: Option<&[u8]>| {
        hasher.update(tag);
        match value {
            Some(value) => {
                hasher.update([1]);
                hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
                hasher.update(value);
            }
            None => hasher.update([0]),
        }
    };
    field(b"shepr-usage-generation-v1", None);
    field(b"provider", Some(provider.tag().as_bytes()));
    field(b"access-token", Some(token));
    field(b"auth-mode", Some(auth_mode.as_bytes()));
    field(
        b"account-id",
        routing.account_id.as_deref().map(str::as_bytes),
    );
    field(b"fedramp", Some(if routing.fedramp { b"1" } else { b"0" }));
    Generation(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(claims: &Value) -> String {
        let encode = |value: &Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(value).expect("json"))
        };
        format!(
            "{}.{}.signature",
            encode(&serde_json::json!({"alg": "none"})),
            encode(claims)
        )
    }

    fn usable(parsed: ParsedCredentials) -> Credential {
        match parsed {
            ParsedCredentials::Usable(credential) => credential,
            other => panic!("expected a usable credential, got {other:?}"),
        }
    }

    #[test]
    fn claude_reads_token_expiry_and_plan_without_the_refresh_token() {
        let file = br#"{"claudeAiOauth":{"accessToken":"at-1","refreshToken":"rt-1",
            "expiresAt":1791378000000,"scopes":["user:inference","user:profile"],
            "subscriptionType":"max"}}"#;
        let credential = usable(parse(Provider::Claude, file).expect("parse"));
        assert_eq!(credential.access_token.expose(), b"at-1");
        assert_eq!(
            credential.expires_at,
            Some(UNIX_EPOCH + Duration::from_secs(1_791_378_000))
        );
        assert_eq!(credential.identity.plan.as_deref(), Some("max"));
        assert!(!format!("{credential:?}").contains("rt-1"));
        assert!(!format!("{credential:?}").contains("at-1"));
    }

    #[test]
    fn claude_without_tokens_is_logged_out_and_setup_tokens_are_unsupported() {
        assert!(matches!(
            parse(Provider::Claude, b"{}"),
            Ok(ParsedCredentials::LoggedOut)
        ));
        assert!(matches!(
            parse(
                Provider::Claude,
                br#"{"claudeAiOauth":{"accessToken":"t","scopes":["user:inference"]}}"#
            ),
            Ok(ParsedCredentials::Unsupported)
        ));
        assert_eq!(
            parse(Provider::Claude, b"{\"claudeAiOauth\":").err(),
            Some(ParseFailure::NotJson)
        );
    }

    #[test]
    fn codex_reads_routing_and_binds_the_id_token_to_the_access_token() {
        let access = jwt(&serde_json::json!({
            "exp": 1_791_378_000,
            "https://api.openai.com/auth": {"chatgpt_user_id": "user-1", "chatgpt_plan_type": "pro"}
        }));
        let id = jwt(&serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_user_id": "user-1"},
            "https://api.openai.com/profile": {"email": "a@example.com"}
        }));
        let file = serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {"access_token": access, "id_token": id, "refresh_token": "rt",
                       "account_id": "acct-9", "chatgpt_account_is_fedramp": true}
        });
        let credential = usable(
            parse(Provider::Codex, &serde_json::to_vec(&file).expect("json")).expect("parse"),
        );
        assert_eq!(credential.routing.account_id.as_deref(), Some("acct-9"));
        assert!(credential.routing.fedramp);
        assert_eq!(credential.identity.principal.as_deref(), Some("user-1"));
        assert_eq!(credential.identity.id_token_bound, Some(true));
        assert_eq!(credential.identity.email.as_deref(), Some("a@example.com"));
        assert_eq!(credential.identity.plan.as_deref(), Some("pro"));
        assert_eq!(
            credential.expires_at,
            Some(UNIX_EPOCH + Duration::from_secs(1_791_378_000))
        );
    }

    #[test]
    fn codex_mismatched_id_token_is_reported_unbound() {
        let access = jwt(&serde_json::json!({"sub": "user-1"}));
        let id = jwt(&serde_json::json!({"sub": "user-2"}));
        let file = serde_json::json!({"tokens": {"access_token": access, "id_token": id}});
        let credential = usable(
            parse(Provider::Codex, &serde_json::to_vec(&file).expect("json")).expect("parse"),
        );
        assert_eq!(credential.identity.id_token_bound, Some(false));
    }

    #[test]
    fn codex_api_key_and_signed_out_files_are_told_apart() {
        assert!(matches!(
            parse(
                Provider::Codex,
                br#"{"OPENAI_API_KEY":"sk-x","tokens":null}"#
            ),
            Ok(ParsedCredentials::ApiKey)
        ));
        assert!(matches!(
            parse(Provider::Codex, br#"{"tokens":null}"#),
            Ok(ParsedCredentials::LoggedOut)
        ));
    }

    #[test]
    fn the_generation_changes_with_any_routing_field() {
        let routing = Routing {
            account_id: Some("a".into()),
            fedramp: false,
        };
        let base = generation(Provider::Codex, b"t", &routing, "chatgpt");
        assert_eq!(base, generation(Provider::Codex, b"t", &routing, "chatgpt"));
        assert_ne!(base, generation(Provider::Codex, b"u", &routing, "chatgpt"));
        assert_ne!(
            base,
            generation(Provider::Claude, b"t", &routing, "chatgpt")
        );
        let other_account = Routing {
            account_id: None,
            ..routing.clone()
        };
        assert_ne!(
            base,
            generation(Provider::Codex, b"t", &other_account, "chatgpt")
        );
        let fedramp = Routing {
            fedramp: true,
            ..routing
        };
        assert_ne!(base, generation(Provider::Codex, b"t", &fedramp, "chatgpt"));
    }
}
