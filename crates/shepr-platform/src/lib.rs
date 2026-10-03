//! Linux process, terminal, filesystem and transport primitives.
//!
//! The modules stay flat by responsibility; higher-level rules belong to the
//! crates that consume these primitives.

mod child_io;
mod client_stream;
mod clipboard;
mod config_file;
mod daemon;
mod data_directory_lease;
mod executable;
mod file_stamp;
mod host;
pub mod ipc;
mod limits;
pub mod logging;
mod owned_runtime;
mod private_file;
mod proc_tree;
mod process;
mod process_identity;
pub mod publish_file;
mod random;
mod remote_bridge;
mod remote_bridge_io;
mod ssh_paths;
mod stderr_null;
mod terminal_environment;

pub use child_io::{
    ChildExitReason, classify_child_exit, poll_fd_readable, poll_timeout_until, read_fd,
    set_cloexec, set_fd_nonblocking, set_nonblocking,
};
pub use client_stream::{ClientStreamReader, wait_client_stream_readable, write_client_stream};
pub use clipboard::{read_clipboard_text, write_clipboard};
pub use config_file::{config_file_link_count, create_config_temporary, write_config_temporary};
pub use daemon::{SpawnedDaemon, create_private_directory_all, open_boot_log, read_boot_log_tail};
pub use data_directory_lease::{DataDirectoryLease, DataDirectoryLeaseHeld, LeaseAcquireError};
pub use executable::{
    ExecutableStatus, classify_executable, has_execute_access, is_pane_shell_process_name,
};
pub use file_stamp::FileStamp;
pub use host::{
    begin_cli_output, child_command, detach_server_daemon_command, hostname, launch_executable,
    take_terminal_resize_signal, terminal_grid_size, watch_terminal_resize_signal,
};
pub use owned_runtime::{release_remote_ssh_config_dir, release_single_use_socket_lock};
pub use private_file::{create_private_file, open_regular_file, sync_directory};
pub use proc_tree::{
    ForegroundJob, ForegroundProcess, foreground_group_leader_job, foreground_job,
    foreground_process_group_id, process_cwd, suspended_processes,
};
pub use process::{
    Pgid, Pid, ProcStat, ProcState, ProcessHandle, SessionId, Signal, reap_pidfd,
    session_member_handles, session_members, wait_for_process_exits,
};
pub use random::unpredictable_token;
pub use remote_bridge_io::{
    RemoteBridgeOutcome, RemoteBridgeWake, answer_remote_bridge, forward_remote_bridge_stdio,
};
pub use ssh_paths::UnsafeSshRuntimeDirectory;
pub use ssh_paths::{
    RemoteSshConfigPaths, SshControlKey, SshRuntimeError, create_remote_ssh_config_dir,
    remote_bridge_endpoint_path, remote_ssh_config_file_path, remote_ssh_config_paths,
    shared_ssh_control_path, ssh_control_path_under, validate_remote_bridge_endpoint_path,
    validate_ssh_runtime_dir,
};
pub use stderr_null::redirect_stderr_to_null;
pub use terminal_environment::prefers_osc52_clipboard;

/// Whether a presence variable (`shepr_core::env::EnvKind::Presence`) is set,
/// for per-call host probes with no error path to report through. Presence
/// reads inspect only raw non-emptiness, so padding and non-UTF-8 bytes are
/// accepted; the fallback keeps a future registry error fail-soft.
fn env_present(var: shepr_core::env::EnvVar) -> bool {
    shepr_core::env::read_present(var).unwrap_or_else(|error| {
        tracing::warn!(%error, "ignoring a refused environment value");
        false
    })
}

// Shared helpers for sibling platform modules.
use child_io::{LimitedRead, poll_fd, read_limited_reader};
use host::effective_uid;

#[cfg(test)]
use clipboard::{
    ClipboardCommand, ClipboardSession, clipboard_commands, read_clipboard_text_commands,
    read_clipboard_text_with_command, read_clipboard_text_with_command_with_clock,
    run_clipboard_command, run_clipboard_command_with_clock, write_clipboard_with,
};
#[cfg(test)]
use host::resolve_launch_executable;
#[cfg(test)]
use limits::CLIPBOARD_HELPER_TIMEOUT;
#[cfg(test)]
use process::process_exists;
#[cfg(test)]
use process::session_and_tty_from_stat;
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
