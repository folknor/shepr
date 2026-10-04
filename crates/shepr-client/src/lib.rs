//! Thin client mode - connects to the server socket.
//!
//! The client:
//! - Connects to `shepr.sock`, sends the build preamble and terminal geometry, then reads
//!   the server's preamble
//! - Sets up the real terminal (raw mode, mouse capture, keyboard enhancements)
//! - Receives surface messages, composes them with the client shell chrome and blits the
//!   result to the terminal (diff against last frame)
//! - Reads stdin events (keystrokes, mouse, paste), routes them through the client shell and
//!   sends pane input as ClientMessage::ClientShellPaneInput
//! - Detects terminal resize and sends ClientMessage::ClientShellResize
//! - Restores terminal on exit (normal or error)
//! - Handles ServerShutdown gracefully (clean exit, informative message returned for the
//!   binary to print once the terminal is restored)
//! - Handles server unreachable (clear error screen, not blank/hang)
//! - Forwards server clipboard writes through the host clipboard helper when available, and
//!   uses OSC 52 when configured or as a fallback
//!
//! The binary launcher installs process-wide file logging before calling the
//! client; client startup reuses that subscriber instead of installing one.

mod client_loop;
mod clipboard_forwarding;
mod deadline;
mod endpoint;
mod errors;
mod events;
mod fatal_panic;
mod handshake;
mod input;
mod input_wire;
mod launch;
mod limits;
mod logging;
mod loop_config;
mod shell;
mod shell_runtime;
mod startup;
mod state;
mod terminal_geometry;
mod terminal_setup;

pub use errors::{ClientExit, ClientRunError};
pub use startup::{run_client, run_client_with_connectors};

#[cfg(test)]
mod tests;
