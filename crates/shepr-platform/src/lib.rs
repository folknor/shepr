//! Linux process, terminal, filesystem and transport primitives.
//!
//! The modules stay flat by responsibility; higher-level rules belong to the
//! crates that consume these primitives.

mod boot_clock;
mod child_io;
mod client_stream;
mod clipboard;
mod config_file;
mod daemon;
mod data_directory_lease;
mod dir_watch;
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
mod stderr_null;
mod stream_wake;
mod structured_log;
mod terminal_environment;

pub use boot_clock::boot_time_nanos;
pub use child_io::{
    ChildExitKind, Wait, classify_child_exit, poll_fd_readable, read_fd, remaining_until,
    set_fd_nonblocking, set_nonblocking,
};
pub use client_stream::{ClientStreamReader, wait_client_stream_readable, write_client_stream};
pub use clipboard::{ClipboardRoute, ClipboardSession, read_clipboard_text};
pub use config_file::config_file_link_count;
pub use daemon::{
    SpawnedDaemon, create_private_directory_all, create_private_runtime_directory, open_boot_log,
    read_boot_log_tail,
};
pub use data_directory_lease::{DataDirectoryLease, DataDirectoryLeaseHeld, LeaseAcquireError};
pub use dir_watch::{DirectoryWake, DirectoryWatch};
pub use executable::{
    ExecutableStatus, classify_executable, has_execute_access, is_pane_shell_process_name,
};
pub use file_stamp::FileStamp;
pub use host::{
    HostNames, begin_cli_output, child_command, detach_server_daemon_command, effective_uid,
    host_names, launch_executable, take_terminal_resize_signal, terminal_grid_size,
    watch_terminal_resize_signal,
};
pub use owned_runtime::{
    DirectoryKind, RuntimeCreateError, create_owned_directory, release_owned_directory,
};
pub use private_file::{
    NotRegularFile, PrivateDirError, create_private_file, open_regular_file,
    require_private_directory, sync_directory,
};
pub use proc_tree::{
    ForegroundJob, ForegroundProcess, foreground_group_leader_job, foreground_job,
    foreground_process_group_id, process_cwd, suspended_processes,
};
pub use process::{
    Pgid, Pid, ProcStat, ProcState, ProcessHandle, SessionId, Signal, reap_pidfd, session_members,
    wait_for_process_exits,
};
pub use random::unpredictable_token;
pub use stderr_null::redirect_stderr_to_null;
pub use stream_wake::StreamWake;
#[doc(hidden)]
pub use structured_log::tracing_backend;

/// The mode of a private runtime directory.
pub const PRIVATE_DIRECTORY_MODE: u32 = limits::PRIVATE_DIRECTORY_MODE;

/// Whether a presence variable (`shepr_core::env::EnvKind::Presence`) is set,
/// for per-call host probes with no error path to report through. Presence
/// reads inspect only raw non-emptiness, so padding and non-UTF-8 bytes are
/// accepted; the fallback keeps a future registry error fail-soft.
fn env_present(var: shepr_core::env::EnvVar) -> bool {
    shepr_core::env::read_present(var).unwrap_or_else(|error| {
        crate::structured_log!(WARN, event = environment.read, outcome = "refused", %error, "ignoring a refused environment value");
        false
    })
}

// Shared helpers for sibling platform modules.
use child_io::{LimitedRead, poll_fd, read_limited_reader};

#[cfg(test)]
use clipboard::{
    ClipboardCommand, clipboard_commands, read_clipboard_text_commands,
    read_clipboard_text_with_command, read_clipboard_text_with_command_with_clock,
    run_clipboard_command, run_clipboard_command_with_clock, write_clipboard_and_primary_with,
};
#[cfg(test)]
use config_file::write_config_temporary;
#[cfg(test)]
use host::resolve_launch_executable;
#[cfg(test)]
use limits::CLIPBOARD_HELPER_TIMEOUT;
#[cfg(test)]
use process::process_exists;
#[cfg(test)]
use process::session_and_tty_from_stat;

#[cfg(test)]
mod resize_signal_tests;
#[cfg(test)]
mod tests;
