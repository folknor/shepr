//! Linux process, terminal, filesystem and transport primitives.
//!
//! The modules stay flat by responsibility; higher-level rules belong to the
//! crates that consume these primitives.

mod child_io;
mod client_stream;
mod clipboard;
mod config_file;
mod executable;
pub mod git;
mod host;
pub mod ipc;
mod limits;
pub mod logging;
mod private_file;
mod process;
mod process_identity;
mod random;
mod remote_bridge;
mod remote_bridge_io;
mod shutdown;
pub mod ssh_agent;
mod ssh_paths;
mod terminal_environment;

pub use child_io::{ChildExitReason, classify_child_exit, poll_fd_readable, read_fd};
pub use client_stream::{ClientStreamReader, wait_client_stream_readable, write_client_stream};
pub use clipboard::{read_clipboard_text, write_clipboard};
pub use config_file::{config_file_link_count, create_config_temporary, write_config_temporary};
pub use executable::has_execute_access;
pub use host::{
    begin_cli_output, child_command, current_process_is_detached_server_daemon,
    detach_server_daemon_command, hostname, launch_executable, local_datetime,
    take_terminal_resize_signal, terminal_grid_size, watch_terminal_resize_signal,
};
pub use private_file::{create_private_file, sync_directory};
pub use process::{
    ProcessHandle, Signal, reap_pidfd, session_member_handles, wait_for_process_exits,
};
pub use random::unpredictable_token;
pub use remote_bridge_io::{RemoteBridgeOutcome, RemoteBridgeWake, forward_remote_bridge_stdio};
pub use shutdown::HostShutdownMonitor;
pub use ssh_paths::UnsafeSshRuntimeDirectory;
pub use ssh_paths::{
    RemoteSshConfigPaths, create_remote_ssh_config_dir, remote_bridge_endpoint_path,
    remote_ssh_config_paths, shared_ssh_control_path, ssh_control_path_under,
    validate_ssh_runtime_dir,
};
pub use terminal_environment::prefers_osc52_clipboard;

/// Whether a presence variable (`shepr_core::env::EnvKind::Presence`) is set,
/// for the per-call host probes that have no error path to report through: a
/// refused value (padded or non-UTF-8) is logged and reads as unset.
fn env_present(var: shepr_core::env::EnvVar) -> bool {
    shepr_core::env::read_present(var).unwrap_or_else(|error| {
        tracing::warn!(%error, "ignoring a refused environment value");
        false
    })
}

// Shared helpers for sibling platform modules.
use child_io::{LimitedRead, poll_fd, poll_timeout_until, read_limited_reader};
use host::effective_uid;
use process::session_and_tty_from_stat;

#[cfg(test)]
use clipboard::{
    ClipboardCommand, ClipboardSession, clipboard_commands, read_clipboard_text_commands,
    read_clipboard_text_with_command, read_clipboard_text_with_command_with_clock,
    run_clipboard_command, run_clipboard_command_with_clock, write_clipboard_with,
};
#[cfg(test)]
use host::{is_detached_session, resolve_launch_executable};
#[cfg(test)]
use limits::CLIPBOARD_HELPER_TIMEOUT;
#[cfg(test)]
use process::process_exists;
#[cfg(test)]
use remote_bridge_io::forward_remote_bridge_stdio_with_timeout;
#[cfg(test)]
use ssh_paths::{validate_shared_ssh_dir, with_name_token};

#[cfg(test)]
mod remote_bridge_tests;
#[cfg(test)]
mod resize_signal_tests;
#[cfg(test)]
mod tests;
