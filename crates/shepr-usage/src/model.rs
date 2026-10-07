//! What the worker publishes: accounts with their latest measurement and the
//! health of polling them, and every source with what its credentials say.
//! Nothing here holds a credential, and every time is a wall-clock instant,
//! so freshness is read at display time from `observed_at` and `resets_at`.

use std::time::{Duration, SystemTime};

use crate::credentials::Generation;
use crate::source::{Provider, SourceLocator, SourceOrigin};

/// One published state of the worker. `incarnation` changes when a worker is
/// replaced, `revision` with every change within one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageSnapshot {
    pub incarnation: u64,
    pub revision: u64,
    pub accounts: Vec<AccountView>,
    pub sources: Vec<SourceView>,
    pub transport: TransportHealth,
    /// Whether remembered sources reach disk.
    pub registry: RegistryHealth,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RegistryHealth {
    /// No registry file is configured.
    #[default]
    Disabled,
    Ok,
    /// The last write failed; remembered sources live only in memory.
    WriteFailed,
    /// The file could not be read at start; it is rewritten on the next
    /// change.
    ReadFailed,
}

/// Who a measurement belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AccountKey {
    /// From an authenticated `/profile`, with the same token as `/usage`.
    Claude {
        account_uuid: String,
        organization_uuid: String,
    },
    /// The principal the access token names and the selected account.
    Codex {
        principal: String,
        account_id: String,
    },
    /// A credential whose account could not be established; its measurements
    /// are never merged with another's.
    Unresolved {
        provider: Provider,
        generation: Generation,
    },
}

impl AccountKey {
    pub fn provider(&self) -> Provider {
        match self {
            Self::Claude { .. } => Provider::Claude,
            Self::Codex { .. } => Provider::Codex,
            Self::Unresolved { provider, .. } => *provider,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountView {
    pub key: AccountKey,
    /// An email or display name, when one is known. Never part of the key.
    pub label: Option<String>,
    /// Plan facts as the provider reports them.
    pub plan: Plan,
    pub measurement: Option<Measurement>,
    /// Whether any source supplies a credential that may serve a request now.
    /// Independent of `usage`: an account can have a usable credential and be
    /// throttled, or lose its credentials with its last failure still shown.
    pub has_usable_credential: bool,
    /// How polling usage went. Credential-specific refusals (401, 403) show
    /// on the source, never here.
    pub usage: EndpointHealth,
    /// Claude only: the profile endpoint's health.
    pub profile: Option<EndpointHealth>,
    /// The sources currently supplying this account's credentials.
    pub sources: Vec<SourceLocator>,
    /// Why the account could not be established, for an
    /// [`AccountKey::Unresolved`] account.
    pub identity_problem: Option<IdentityProblem>,
}

/// Plan facts, verbatim.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Claude `organization_type`, or the Codex `plan_type`.
    pub kind: Option<String>,
    /// Claude `rate_limit_tier`.
    pub tier: Option<String>,
    /// Claude `subscription_status`.
    pub status: Option<String>,
}

/// One whole usage response. A newer one replaces it entirely: a window the
/// newer response lacks is withdrawn, never carried over.
#[derive(Debug, Clone, PartialEq)]
pub enum Measurement {
    Claude(ClaudeMeasurement),
    Codex(CodexMeasurement),
}

impl Measurement {
    /// When the response was received.
    pub fn observed_at(&self) -> SystemTime {
        match self {
            Self::Claude(measurement) => measurement.observed_at,
            Self::Codex(measurement) => measurement.observed_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeMeasurement {
    pub observed_at: SystemTime,
    pub limits: Vec<ClaudeLimit>,
}

/// One limit of a Claude usage response.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeLimit {
    /// `limits[].kind` verbatim, or `five_hour`/`seven_day` for a legacy block.
    pub kind: String,
    pub model: Option<String>,
    pub surface: Option<String>,
    /// Derived only for the recognized kinds (the session and weekly ones).
    pub duration: Option<Duration>,
    /// Percent used; `None` when missing or invalid, never zero by default.
    pub used_percent: Option<f64>,
    pub resets_at: Option<SystemTime>,
    /// `is_active` as reported; its meaning is unverified.
    pub is_active: Option<bool>,
    /// Filled from a legacy top-level block because `limits[]` lacked it.
    pub legacy: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodexMeasurement {
    pub observed_at: SystemTime,
    pub plan_type: Option<String>,
    pub groups: Vec<CodexGroup>,
    /// `rate_limit_reached_type.type` verbatim, unknown values included.
    pub reached_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexGroupId {
    /// The top-level `rate_limit`.
    Main,
    /// One of `additional_rate_limits`.
    Additional {
        limit_name: String,
        metered_feature: String,
    },
}

/// A rate-limit group: its own verdict and up to two windows.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexGroup {
    pub id: CodexGroupId,
    /// `None` when missing; never read as false.
    pub allowed: Option<bool>,
    pub limit_reached: Option<bool>,
    pub primary: Option<CodexWindow>,
    pub secondary: Option<CodexWindow>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodexWindow {
    pub used_percent: Option<f64>,
    pub limit_window: Option<Duration>,
    pub reset_at: Option<SystemTime>,
    /// As received; relative to `observed_at` of the measurement.
    pub reset_after: Option<Duration>,
}

/// How polling one endpoint of an account is going.
#[derive(Debug, Clone, PartialEq)]
pub enum EndpointHealth {
    /// Not polled yet.
    Pending,
    /// The last poll succeeded.
    Ok { at: SystemTime },
    /// Throttled (429); the next poll is not before `retry_at`.
    Throttled { retry_at: SystemTime, streak: u32 },
    /// Network, transport, server or response failure; retried with backoff.
    Failing {
        failure: FailureClass,
        retry_at: SystemTime,
        streak: u32,
    },
    /// A request to this host is still held by a child that has not been
    /// reaped, so no new one starts.
    HostBlocked,
}

/// Why a poll failed, without any response or error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    Dns,
    Connect,
    Tls,
    Timeout,
    /// A status other than the ones classified elsewhere.
    Status(u16),
    /// The response could not be used: oversize, bad framing, unexpected
    /// encoding, or a body that is not the expected JSON.
    Response,
    /// curl failed in some other way.
    Transport,
}

/// Every discovered source and what its credentials say.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceView {
    pub locator: SourceLocator,
    pub origin: SourceOrigin,
    pub availability: Availability,
    /// The account its current credential supplies, once known.
    pub account: Option<AccountKey>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Availability {
    NotYetRead,
    /// Supplies a credential, valid until `expires_at` when known.
    Usable {
        expires_at: Option<SystemTime>,
    },
    /// The access token's expiry has passed; the agent renews it when it runs.
    Expired,
    /// The provider rejected this credential (401); waiting for it to change.
    Rejected,
    /// Refused for this credential (403); waiting for it to change.
    Denied,
    LoggedOut,
    /// An API-key login, which has no subscription limits.
    ApiKey,
    /// A credential shape or token kind this cannot use.
    Unsupported,
    /// No credentials file.
    Missing,
    /// The file could not be read or parsed. Requests stopped at the first
    /// failure; `persistent` once the grace has passed.
    Unreadable {
        reason: ReadFailure,
        persistent: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadFailure {
    /// Not a regular file, or owned by another user.
    NotAcceptable,
    TooLarge,
    Io(std::io::ErrorKind),
    NotJson,
    /// The read has not finished within its deadline.
    Stalled,
    /// Every reader slot is taken by reads still running.
    ReadersExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityProblem {
    /// Codex: the token claims name no principal or account.
    MissingClaims,
    /// Codex: the id token names a different principal from the access token.
    IdTokenMismatch,
    /// Claude: `/profile` lacked the account or organization uuid.
    ProfileIncomplete,
}

/// Whether requests can be made at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum TransportHealth {
    /// Not probed yet (nothing has needed a request).
    #[default]
    Unprobed,
    Ready,
    /// No acceptable curl executable was found.
    Missing,
    /// A curl was found but its probe failed or it lacks a needed capability.
    Unusable,
}
