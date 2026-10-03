//! The server lifecycle seen from outside the server: how a client finds,
//! starts, probes, restarts and stops one, the words it says about doing so,
//! and the vocabulary every endpoint failure is classified in.
//!
//! - [`local_server`] finds the local server or starts the `shepr-server`
//!   beside the running client and waits until it proves its build.
//! - [`status`] probes a server socket for presence and readiness, the
//!   question that decides whether a launch is permitted.
//! - [`stop`] stops a server, conditionally on the boot that was observed.
//! - [`restart`] is the consent-driven restart offer for a server of another
//!   build, local or remote.
//! - [`invocation`] and [`daemon_exit`] are the command lines and exit codes
//!   shepr processes speak to each other: the server executable's arguments
//!   and exit classes, and the CLI's command words.
//! - [`guidance`] names the commands that reach a server.
//! - [`failure`] is the endpoint failure vocabulary and its one disposition
//!   table, for every endpoint, local or SSH.
//! - [`connection_health`] is the heartbeat cadence a connected client keeps,
//!   which anything relaying the connection must outlast.

pub mod connection_health;
pub mod daemon_exit;
pub mod failure;
pub mod guidance;
pub mod invocation;
mod limits;
pub mod local_server;
pub mod restart;
pub mod status;
pub mod stop;
mod text;

pub use failure::{
    EndpointFailure, FailureCause, FailureDisposition, RemoteFailureClass, SshFailureClass,
};
pub use text::RemoteText;
