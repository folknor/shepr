//! Platform log levels: debug is routine diagnostic detail; info records
//! lifecycle and successful state changes; warn records recoverable failures
//! or degraded operation; error records operations that failed or state that
//! could not be preserved.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriter;

const DEFAULT_MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
/// One previous generation (`<name>.1`) survives a rotation, so the lines
/// leading up to it are not lost the moment the limit is hit.
const DEFAULT_RETAINED_LOG_FILES: usize = 1;

/// The filter every file log starts with when `SHEPR_LOG` is unset or empty.
const DEFAULT_LOG_FILTER: &str = "shepr=info";

/// The validated filter for one process's file logger.
pub struct FileLoggingConfig {
    filter: EnvFilter,
}

impl FileLoggingConfig {
    /// Reads and validates `SHEPR_LOG` once for this process launch.
    pub fn from_environment() -> io::Result<Self> {
        let directives = shepr_core::env::read_text(shepr_core::env::EnvVar::SheprLog)?;
        Ok(Self {
            filter: log_filter(directives.as_deref())?,
        })
    }
}

/// Installs the file logger.
///
/// # Errors
///
/// A `SHEPR_LOG` the environment policy refuses, or one that is not valid
/// `tracing` filter syntax, fails the launch rather than silently logging at
/// the default level. A log file that cannot be opened is reported on stderr
/// and the process runs without file logging. An already-installed global
/// logger also fails initialization because this call cannot install its file
/// writer.
pub fn init_file_logging(dir: &Path, file_name: &str) -> io::Result<()> {
    init_file_logging_with_config(dir, file_name, FileLoggingConfig::from_environment()?)
}

/// Installs the file logger with a filter already validated for this process.
pub fn init_file_logging_with_config(
    dir: &Path,
    file_name: &str,
    config: FileLoggingConfig,
) -> io::Result<()> {
    let filter = config.filter;

    let make_writer = match RotatingFileMakeWriter::new(
        dir,
        file_name,
        DEFAULT_MAX_LOG_BYTES,
        DEFAULT_RETAINED_LOG_FILES,
    ) {
        Ok(make_writer) => make_writer,
        Err(error) => {
            // With stderr unwritable too there is nowhere left to report
            // to, and running without file logging is the documented outcome.
            writeln!(
                io::stderr().lock(),
                "shepr: could not initialize file logging: {error}"
            )
            .ok();
            return Ok(());
        }
    };

    // A global subscriber (or `log` logger) already installed means this
    // process cannot honor the file logger setup requested by this call.
    if let Err(error) = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(make_writer)
        .with_ansi(false)
        .with_target(true)
        .try_init()
    {
        tracing::warn!(
            file = file_name,
            dir = %dir.display(),
            err = %error,
            "file logging not installed: a logger is already set"
        );
        return Err(io::Error::other(format!(
            "file logging could not be initialized because a logger is already set: {error}"
        )));
    }
    Ok(())
}

/// The file-log filter from `SHEPR_LOG`'s value (already read under the
/// environment policy), or the default when it is unset.
fn log_filter(directives: Option<&str>) -> io::Result<EnvFilter> {
    let Some(directives) = directives else {
        return Ok(EnvFilter::new(DEFAULT_LOG_FILTER));
    };
    EnvFilter::try_new(directives).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is {directives:?}, which is not a valid log filter: {error}",
                shepr_core::env::EnvVar::SheprLog
            ),
        )
    })
}

/// The log the headless server writes.
pub const SERVER_LOG_FILE: &str = "shepr-server.log";
/// The log every client process appends to.
pub const CLIENT_LOG_FILE: &str = "shepr-client.log";

/// Installs the process-wide client file logger from the binary launch path.
/// The client library reuses this subscriber and does not install one itself.
pub fn init_client_file_logging(dir: &Path, config: FileLoggingConfig) -> io::Result<()> {
    init_file_logging_with_config(dir, CLIENT_LOG_FILE, config)
}

/// The log files `--help` names: the only two any process writes.
pub fn help_log_paths_summary(dir: &Path) -> String {
    log_paths_summary(dir)
}

fn log_paths_summary(dir: &Path) -> String {
    format!(
        "{} (and {CLIENT_LOG_FILE} beside it)",
        dir.join(SERVER_LOG_FILE).display()
    )
}

pub fn startup(role: &'static str) {
    // The PID is event identity for correlating each process's lifecycle rows.
    tracing::info!(
        event = "app.startup",
        subsystem = role,
        outcome = "started",
        pid = std::process::id(),
        "shepr starting"
    );
}

pub fn shutdown(role: &'static str) {
    tracing::info!(
        event = "app.shutdown",
        subsystem = role,
        outcome = "completed",
        pid = std::process::id(),
        "shepr exiting"
    );
}

pub fn api_request_started(request_id: &str, method_name: &str, mutates_ui: bool, routine: bool) {
    let event = "api.request.start";
    let subsystem = "api";
    let outcome = "started";
    let message = "api request received";
    if mutates_ui && !routine {
        tracing::info!(
            event,
            subsystem,
            outcome,
            request_id,
            method = method_name,
            changes_ui = mutates_ui,
            "{message}"
        );
    } else {
        tracing::debug!(
            event,
            subsystem,
            outcome,
            request_id,
            method = method_name,
            changes_ui = mutates_ui,
            "{message}"
        );
    }
}

pub fn api_request_completed(
    request_id: &str,
    method_name: &str,
    mutates_ui: bool,
    routine: bool,
    outcome: &'static str,
) {
    let event = "api.request.complete";
    let subsystem = "api";
    let message = "api request completed";
    if outcome != "ok" || (mutates_ui && !routine) {
        tracing::info!(
            event,
            subsystem,
            outcome,
            request_id,
            method = method_name,
            "{message}"
        );
    } else {
        tracing::debug!(
            event,
            subsystem,
            outcome,
            request_id,
            method = method_name,
            "{message}"
        );
    }
}

pub fn api_request_failed(request_id: &str, method_name: &str, err: &str) {
    tracing::error!(
        event = "api.request.fail",
        subsystem = "api",
        outcome = "error",
        request_id,
        method = method_name,
        err,
        "api request failed"
    );
}

pub fn api_wait_started(request_id: &str, pane_id: &str, timeout_ms: Option<u64>) {
    tracing::info!(
        event = "api.wait.start",
        subsystem = "api",
        outcome = "started",
        request_id,
        pane_id,
        timeout_ms,
        "api output wait started"
    );
}

pub fn api_wait_completed(request_id: &str, pane_id: &str, outcome: &'static str) {
    tracing::info!(
        event = "api.wait.complete",
        subsystem = "api",
        outcome,
        request_id,
        pane_id,
        "api output wait finished"
    );
}

pub fn api_wait_timed_out(request_id: &str, pane_id: &str) {
    tracing::warn!(
        event = "api.wait.timeout",
        subsystem = "api",
        outcome = "timeout",
        request_id,
        pane_id,
        "api output wait timed out"
    );
}

pub fn pane_spawn_started(pane_id: u32, rows: u16, cols: u16, scrollback_limit_bytes: usize) {
    tracing::info!(
        event = "pane.spawn.start",
        subsystem = "pane",
        outcome = "started",
        pane_id,
        rows,
        cols,
        scrollback_limit_bytes,
        "spawning pane terminal"
    );
}

pub fn pane_spawned(pane_id: u32, pid: u32) {
    tracing::info!(
        event = "pane.spawned",
        subsystem = "pane",
        outcome = "ok",
        pane_id,
        pid,
        "pane child spawned"
    );
}

pub fn pane_exited(pane_id: u32, status: &str) {
    tracing::info!(
        event = "pane.exit",
        subsystem = "pane",
        outcome = "completed",
        pane_id,
        status,
        "pane child exited"
    );
}

pub fn pane_exit_failed(pane_id: u32, err: &str) {
    tracing::error!(
        event = "pane.exit",
        subsystem = "pane",
        outcome = "error",
        pane_id,
        err,
        "pane child wait failed"
    );
}

pub fn workspace_created(workspace_id: &str, root_pane_id: u32) {
    tracing::info!(
        event = "workspace.create",
        subsystem = "workspace",
        outcome = "ok",
        workspace_id,
        pane_id = root_pane_id,
        "workspace created"
    );
}

pub fn workspace_focused(workspace_id: &str) {
    tracing::info!(
        event = "workspace.focus",
        subsystem = "workspace",
        outcome = "ok",
        workspace_id,
        "workspace focused"
    );
}

pub fn workspace_closed(workspace_id: &str) {
    tracing::info!(
        event = "workspace.close",
        subsystem = "workspace",
        outcome = "ok",
        workspace_id,
        "workspace closed"
    );
}

pub fn workspace_renamed(workspace_id: &str) {
    tracing::info!(
        event = "workspace.rename",
        subsystem = "workspace",
        outcome = "ok",
        workspace_id,
        "workspace renamed"
    );
}

pub fn tab_focused(workspace_id: &str, tab_id: &str) {
    tracing::info!(
        event = "tab.focus",
        subsystem = "tab",
        outcome = "ok",
        workspace_id,
        tab_id,
        "tab focused"
    );
}

pub fn tab_closed(workspace_id: &str, tab_id: &str) {
    tracing::info!(
        event = "tab.close",
        subsystem = "tab",
        outcome = "ok",
        workspace_id,
        tab_id,
        "tab closed"
    );
}

pub fn tab_renamed(workspace_id: &str, tab_id: &str) {
    tracing::info!(
        event = "tab.rename",
        subsystem = "tab",
        outcome = "ok",
        workspace_id,
        tab_id,
        "tab renamed"
    );
}

pub fn session_saved(path: &Path, workspaces: usize) {
    tracing::info!(
        event = "persist.save",
        subsystem = "persist",
        outcome = "ok",
        path = %path.display(),
        workspaces,
        "session saved"
    );
}

pub fn session_save_failed(path: &Path, err: &str) {
    tracing::error!(
        event = "persist.save",
        subsystem = "persist",
        outcome = "error",
        path = %path.display(),
        err,
        "failed to save session"
    );
}

pub fn session_cleared(path: &Path) {
    tracing::info!(
        event = "persist.clear",
        subsystem = "persist",
        outcome = "ok",
        path = %path.display(),
        "session cleared"
    );
}

pub fn session_clear_failed(path: &Path, err: &str) {
    tracing::error!(
        event = "persist.clear",
        subsystem = "persist",
        outcome = "error",
        path = %path.display(),
        err,
        "failed to clear session"
    );
}

pub fn session_restored(path: &Path, session_id: &str, workspaces: usize, outcome: &'static str) {
    tracing::info!(
        event = "persist.restore",
        subsystem = "persist",
        outcome,
        session_id,
        path = %path.display(),
        workspaces,
        "session restore evaluated"
    );
}

pub fn integration_action(action: &'static str, target: &'static str, outcome: &'static str) {
    tracing::info!(
        event = "integration.action",
        subsystem = "integration",
        outcome,
        action,
        target,
        "integration action finished"
    );
}

struct RotatingFileMakeWriter {
    state: Arc<Mutex<RotatingFileState>>,
}

impl RotatingFileMakeWriter {
    fn new(dir: &Path, file_name: &str, max_bytes: u64, retained_files: usize) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let path = dir.join(file_name);
        let state = RotatingFileState {
            path,
            max_bytes,
            retained_files,
            lost_reason: None,
        };
        state.open_current_file()?;
        Ok(Self {
            state: Arc::new(Mutex::new(state)),
        })
    }
}

impl<'a> MakeWriter<'a> for RotatingFileMakeWriter {
    type Writer = RotatingFileGuard;

    fn make_writer(&'a self) -> Self::Writer {
        RotatingFileGuard {
            state: Arc::clone(&self.state),
        }
    }
}

struct RotatingFileGuard {
    state: Arc<Mutex<RotatingFileState>>,
}

impl Write for RotatingFileGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let (operation, pending_reason) = {
            let mut state = self.lock_state();
            let pending_reason = state.lost_reason.take();
            (state.clone(), pending_reason)
        };

        // The state mutex protects only this snapshot and recovery marker.
        // File opens, flock, rotation and writes use the owned snapshot after
        // the guard has been dropped.
        let result = operation
            .write_once(buf, pending_reason.as_deref())
            .or_else(|_| operation.write_once(buf, pending_reason.as_deref()));
        match result {
            Ok(written) => Ok(written),
            Err(error) => {
                let mut state = self.lock_state();
                if state.lost_reason.is_none() || pending_reason.is_some() {
                    state.lost_reason = Some(pending_reason.unwrap_or_else(|| error.to_string()));
                }
                Ok(buf.len())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        // Each write reaches the file directly; std::fs::File has no buffered
        // userspace data to flush here.
        Ok(())
    }
}

impl RotatingFileGuard {
    fn lock_state(&self) -> MutexGuard<'_, RotatingFileState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                self.state.clear_poison();
                if state.lost_reason.is_none() {
                    state.lost_reason = Some("file logging state mutex was poisoned".to_owned());
                }
                state
            }
        }
    }
}

/// Configuration and recovery state for one log file shared by processes.
/// Every write opens the current path and checks its inode, so another process
/// can rotate without leaving this writer appending to an unlinked generation.
#[derive(Clone)]
struct RotatingFileState {
    path: PathBuf,
    max_bytes: u64,
    retained_files: usize,
    /// The reason for an ongoing logging gap, reported once writing works
    /// again.
    lost_reason: Option<String>,
}

/// Log files hold pane activity and error details; keep them private to the
/// user like the rest of the data directory's state.
const LOG_FILE_MODE: u32 = 0o600;

impl RotatingFileState {
    /// Write one chunk without holding the shared recovery-state mutex through
    /// filesystem calls. File-level shared/exclusive flocks coordinate writes
    /// and rotation across processes.
    fn write_once(&self, buf: &[u8], pending_reason: Option<&str>) -> io::Result<usize> {
        let resumed = if let Some(reason) = pending_reason {
            let mut message =
                format!("shepr: file logging resumed; log lines may have been lost: {reason}\n")
                    .into_bytes();
            message.extend_from_slice(buf);
            Some(message)
        } else {
            None
        };
        let write_buf = resumed.as_deref().unwrap_or(buf);
        let incoming_len = u64::try_from(write_buf.len()).unwrap_or(u64::MAX);

        loop {
            let file = self.open_current_file()?;
            let lock = FileLock::shared(&file)?;
            if !self.is_current_file(&file)? {
                drop(lock);
                continue;
            }
            let size = file.metadata()?.len();
            if self.exceeds_limit(size, incoming_len) {
                drop(lock);
                self.rotate_if_needed(incoming_len)?;
                continue;
            }
            let mut append = &file;
            append.write_all(write_buf)?;
            drop(lock);
            return Ok(buf.len());
        }
    }

    fn rotate_if_needed(&self, incoming_len: u64) -> io::Result<()> {
        let file = self.open_current_file()?;
        let lock = FileLock::exclusive(&file)?;
        if !self.is_current_file(&file)? {
            return Ok(());
        }
        if self.exceeds_limit(file.metadata()?.len(), incoming_len) {
            self.rotate_files()?;
        }
        drop(lock);
        Ok(())
    }

    fn exceeds_limit(&self, size: u64, incoming_len: u64) -> bool {
        // An empty file is never rotated, or a single message above the limit
        // would rotate forever.
        self.max_bytes != 0 && size > 0 && size.saturating_add(incoming_len) > self.max_bytes
    }

    fn is_current_file(&self, file: &File) -> io::Result<bool> {
        use std::os::unix::fs::MetadataExt;

        let path_metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let file_metadata = file.metadata()?;
        Ok(
            file_metadata.dev() == path_metadata.dev()
                && file_metadata.ino() == path_metadata.ino(),
        )
    }

    fn open_current_file(&self) -> io::Result<File> {
        use std::os::unix::fs::OpenOptionsExt;

        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(LOG_FILE_MODE)
            .open(&self.path)
    }

    /// Move the current file out of the way (or delete it when no generations
    /// are kept). The caller holds the rotation lock and reopens afterwards.
    fn rotate_files(&self) -> io::Result<()> {
        if self.retained_files == 0 {
            match fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
            return Ok(());
        }

        let oldest = rotated_log_path(&self.path, self.retained_files);
        match fs::remove_file(&oldest) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }

        for index in (1..=self.retained_files).rev() {
            let source = if index == 1 {
                self.path.clone()
            } else {
                rotated_log_path(&self.path, index - 1)
            };
            let target = rotated_log_path(&self.path, index);
            // A missing source is a gap in the rotation, not an error; any
            // other failure is reported rather than read as absence.
            match fs::rename(source, target) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }

        Ok(())
    }
}

/// A shared or exclusive `flock(2)` on an open file, released on drop.
struct FileLock<'a> {
    file: &'a File,
}

impl<'a> FileLock<'a> {
    fn exclusive(file: &'a File) -> io::Result<Self> {
        Self::acquire(file, libc::LOCK_EX)
    }

    fn shared(file: &'a File) -> io::Result<Self> {
        Self::acquire(file, libc::LOCK_SH)
    }

    fn acquire(file: &'a File, operation: libc::c_int) -> io::Result<Self> {
        use std::os::fd::AsRawFd;

        loop {
            // SAFETY: flock(2) on a descriptor owned by `file`, which outlives
            // the returned guard; it touches no memory of this process.
            let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
            if result == 0 {
                return Ok(Self { file });
            }
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
}

impl Drop for FileLock<'_> {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;

        // SAFETY: as in `exclusive`; `self.file` is still open here.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn rotated_log_path(path: &Path, index: usize) -> PathBuf {
    let suffix = format!(".{index}");
    let file_name = path.file_name().map_or_else(
        || suffix.clone().into(),
        |name| {
            let mut name = name.to_os_string();
            name.push(&suffix);
            name
        },
    );
    path.with_file_name(file_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A log path in a fresh scratch directory.
    fn temp_log_path(name: &str) -> PathBuf {
        shepr_test_support::ScratchDir::new(name).join("shepr.log")
    }

    #[test]
    fn log_filter_defaults_when_unset_and_refuses_bad_directives() {
        assert!(log_filter(None).is_ok());
        assert!(log_filter(Some("shepr=debug")).is_ok());
        let error = log_filter(Some("shepr=inof")).expect_err("an unknown level is not a filter");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("SHEPR_LOG"), "{error}");
    }

    #[test]
    fn help_names_only_the_logs_that_are_written() {
        let summary = log_paths_summary(Path::new("/data"));
        assert_eq!(
            summary,
            "/data/shepr-server.log (and shepr-client.log beside it)"
        );
    }

    #[test]
    fn rotated_log_path_appends_numeric_suffix() {
        let path = PathBuf::from("/tmp/shepr.log");
        assert_eq!(
            rotated_log_path(&path, 2),
            PathBuf::from("/tmp/shepr.log.2")
        );
    }

    #[test]
    fn rotate_files_shifts_existing_generations() {
        let path = temp_log_path("rotate");
        fs::write(&path, "current").expect("test precondition");
        fs::write(rotated_log_path(&path, 1), "older").expect("test precondition");

        let state = RotatingFileState {
            path: path.clone(),
            max_bytes: 128,
            retained_files: 2,
            lost_reason: None,
        };
        state.rotate_files().expect("test precondition");

        assert_eq!(
            fs::read_to_string(rotated_log_path(&path, 1)).expect("test precondition"),
            "current"
        );
        assert_eq!(
            fs::read_to_string(rotated_log_path(&path, 2)).expect("test precondition"),
            "older"
        );
        assert!(!path.try_exists().expect("test precondition"));
    }

    #[test]
    fn write_replaces_log_without_retained_files_when_size_limit_is_reached() {
        let path = temp_log_path("replace");
        let dir = path.parent().expect("test precondition").to_path_buf();
        fs::create_dir_all(&dir).expect("test precondition");

        let writer =
            RotatingFileMakeWriter::new(&dir, "shepr.log", 8, 0).expect("test precondition");
        {
            let mut guard = writer.make_writer();
            guard.write_all(b"12345678").expect("test precondition");
            guard.write_all(b"abc").expect("test precondition");
            guard.flush().expect("test precondition");
        }

        assert_eq!(fs::read_to_string(&path).expect("test precondition"), "abc");
        assert!(
            !rotated_log_path(&path, 1)
                .try_exists()
                .expect("test precondition")
        );
    }

    #[test]
    fn writers_sharing_a_log_follow_each_others_rotation() {
        let path = temp_log_path("shared");
        let dir = path.parent().expect("test precondition").to_path_buf();
        fs::create_dir_all(&dir).expect("test precondition");

        // Two processes' writers on the same client log.
        let first = RotatingFileMakeWriter::new(&dir, "shepr.log", 16, 1).expect("first writer");
        let second = RotatingFileMakeWriter::new(&dir, "shepr.log", 16, 1).expect("second writer");

        first
            .make_writer()
            .write_all(b"aaaaaaaaaa")
            .expect("first write");
        // The second writer has written nothing itself, but the file is
        // already 10 bytes: its write crosses the shared limit and rotates.
        second
            .make_writer()
            .write_all(b"bbbbbbbbbb")
            .expect("second write");
        // The first writer must follow the rotation instead of appending to
        // the file that was moved away.
        first.make_writer().write_all(b"cc").expect("third write");

        let current = fs::read_to_string(&path).expect("current log");
        let rotated = fs::read_to_string(rotated_log_path(&path, 1)).expect("rotated log");

        assert_eq!(rotated, "aaaaaaaaaa");
        assert_eq!(current, "bbbbbbbbbbcc");
    }

    #[test]
    fn writer_follows_a_log_deleted_by_another_process() {
        let path = temp_log_path("deleted");
        let dir = path.parent().expect("test precondition").to_path_buf();
        fs::create_dir_all(&dir).expect("test precondition");

        let writer = RotatingFileMakeWriter::new(&dir, "shepr.log", 0, 0).expect("writer");
        writer.make_writer().write_all(b"before").expect("write");
        fs::remove_file(&path).expect("simulated rotation by another process");
        writer.make_writer().write_all(b"after").expect("write");

        let contents = fs::read_to_string(&path);

        assert_eq!(contents.expect("log recreated"), "after");
    }

    #[test]
    fn writer_recovers_after_an_io_error_and_notes_the_gap() {
        let path = temp_log_path("recover");
        let dir = path.parent().expect("test precondition").to_path_buf();
        fs::create_dir_all(&dir).expect("test precondition");

        let writer = RotatingFileMakeWriter::new(&dir, "shepr.log", 0, 0).expect("writer");
        fs::remove_dir_all(&dir).expect("simulated lost log directory");
        // The directory is gone: the write fails, but the caller is not told.
        writer.make_writer().write_all(b"lost").expect("write");
        fs::create_dir_all(&dir).expect("log directory restored");
        writer.make_writer().write_all(b"after").expect("write");

        let contents = fs::read_to_string(&path);

        let contents = contents.expect("log recreated");
        assert!(
            contents.starts_with("shepr: file logging resumed; log lines may have been lost"),
            "{contents}"
        );
        assert!(contents.ends_with("\nafter"), "{contents}");
    }

    #[test]
    fn writer_recovers_after_a_poisoned_mutex_and_notes_the_gap() {
        let path = temp_log_path("poisoned");
        let dir = path.parent().expect("test precondition").to_path_buf();
        fs::create_dir_all(&dir).expect("test precondition");

        let writer = RotatingFileMakeWriter::new(&dir, "shepr.log", 0, 0).expect("writer");
        let poisoner = Arc::clone(&writer.state);
        let joined = std::thread::spawn(move || {
            let _state = poisoner.lock().expect("test lock");
            panic!("simulate a panic while updating file logging state");
        })
        .join();
        assert!(joined.is_err(), "test precondition");
        assert!(writer.state.is_poisoned(), "test precondition");
        writer
            .make_writer()
            .write_all(b"after")
            .expect("poisoned logging state should recover");

        let contents = fs::read_to_string(&path).expect("log recreated");
        assert!(
            contents.starts_with("shepr: file logging resumed; log lines may have been lost: file logging state mutex was poisoned"),
            "{contents}"
        );
        assert!(contents.ends_with("\nafter"), "{contents}");
    }

    #[test]
    fn log_files_are_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_log_path("mode");
        let dir = path.parent().expect("test precondition").to_path_buf();
        fs::create_dir_all(&dir).expect("test precondition");

        let _created = RotatingFileMakeWriter::new(&dir, "shepr.log", 0, 0).expect("writer");
        let created_mode = fs::metadata(&path).expect("log").permissions().mode() & 0o777;

        assert_eq!(created_mode, 0o600);
    }
}
