//! Subscription usage of the Claude and Codex accounts on this host: the
//! remaining five-hour and weekly limits, read from the providers' usage
//! endpoints with the credentials the agents themselves keep.
//!
//! The crate never refreshes a token, never logs in and never writes an
//! agent file. A credential that expires or is rejected stops being used
//! until the agent renews it. Requests go through the system `curl`; nothing
//! TLS is compiled in. Only the server links this crate; it knows nothing of
//! panes, workspaces or clients. The server hands it discovered sources and
//! whether anyone is attached, and reads back [`UsageSnapshot`]s.
//!
//! The endpoints are undocumented. Every shape is read leniently and every
//! missing value stays unknown: a missing percent is never zero, a missing
//! flag is never false, and a window absent from a newer response is
//! withdrawn rather than carried over.

mod claude_api;
mod codex_api;
mod credentials;
mod limits;
mod model;
mod reader;
mod registry;
mod schedule;
mod secret;
mod source;
mod time;
mod transport;
mod worker;

pub use credentials::Generation;
pub use model::{
    AccountKey, AccountView, Availability, ClaudeLimit, ClaudeMeasurement, CodexGroup,
    CodexGroupId, CodexMeasurement, CodexWindow, EndpointHealth, FailureClass, IdentityProblem,
    Measurement, Plan, ReadFailure, RegistryHealth, SourceView, TransportHealth, UsageSnapshot,
};
pub use source::{
    DiscoveredSource, Provider, SourceLocator, SourceOrigin, Unresolved, resolve_config_directory,
};
pub use worker::{UsageConfig, UsageWorker};
