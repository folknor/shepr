mod alt_screen_read;
pub(crate) mod client_accept;
pub(crate) mod client_commands;
pub(crate) mod client_shell;
#[cfg(feature = "test-api")]
pub mod client_transport;
#[cfg(not(feature = "test-api"))]
pub(crate) mod client_transport;
pub(crate) mod clients;
mod input_wire;
#[cfg(feature = "test-api")]
pub use clients::ClientId;
#[cfg(not(feature = "test-api"))]
pub(crate) use clients::ClientId;
pub mod headless;
pub(crate) mod pane_input;
pub(crate) mod render_stream;
pub mod socket_paths;
