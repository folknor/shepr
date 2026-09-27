use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriter;

const DEFAULT_MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
/// One previous generation (`<name>.1`) survives a rotation, so the lines
/// leading up to it are not lost the moment the limit is hit.
const DEFAULT_RETAINED_LOG_FILES: usize = 1;

pub fn init_file_logging(dir: &Path, file_name: &str) {
    let make_writer = match RotatingFileMakeWriter::new(
        dir,
        file_name,
        DEFAULT_MAX_LOG_BYTES,
        DEFAULT_RETAINED_LOG_FILES,
    ) {
        Ok(make_writer) => make_writer,
        Err(error) => {
            let _ = writeln!(
                io::stderr().lock(),
                "shepr: could not initialize file logging: {error}"
            );
            return;
        }
    };

    let filter =
        EnvFilter::try_from_env("SHEPR_LOG").unwrap_or_else(|_| EnvFilter::new("shepr=info"));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(make_writer)
        .with_ansi(false)
        .with_target(true)
        .try_init();
}

/// The log the headless server writes.
pub const SERVER_LOG_FILE: &str = "shepr-server.log";
/// The log every client process appends to.
pub const CLIENT_LOG_FILE: &str = "shepr-client.log";

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
    tracing::warn!(
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

pub fn session_restored(workspaces: usize, outcome: &'static str) {
    tracing::info!(
        event = "persist.restore",
        subsystem = "persist",
        outcome,
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
        let mut state = RotatingFileState {
            path,
            max_bytes,
            retained_files,
            file: None,
            lost_error: None,
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
        let Ok(mut state) = self.state.lock() else {
            return Ok(buf.len());
        };
        match state.write_with_recovery(buf) {
            Ok(written) => Ok(written),
            Err(_) => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let Ok(mut state) = self.state.lock() else {
            return Ok(());
        };
        state.flush_with_recovery();
        Ok(())
    }
}

/// One log file that may be shared by several processes: every client appends
/// to the same `shepr-client.log`. Nothing about the file is cached per
/// process. Before each write the path is checked against the open file, so a
/// rotation done by another process is noticed and followed instead of leaving
/// this one writing to an unlinked inode, and the size limit is judged on the
/// file's real size rather than on this process's own share of it.
struct RotatingFileState {
    path: PathBuf,
    max_bytes: u64,
    retained_files: usize,
    file: Option<File>,
    /// The first write error of an ongoing outage, reported in the log once
    /// writing works again.
    lost_error: Option<String>,
}

/// Log files hold pane activity and error details; keep them private to the
/// user like the rest of the data directory's state.
const LOG_FILE_MODE: u32 = 0o600;

impl RotatingFileState {
    /// Write one chunk, reopening the file once on failure. A failed write
    /// never disables logging: the next event tries again, so a full disk or
    /// a removed directory recovers once the cause is gone. The first error of
    /// an outage is kept and written into the log when writing resumes; it is
    /// not sent to stderr, which is the client's TUI terminal.
    fn write_with_recovery(&mut self, buf: &[u8]) -> io::Result<usize> {
        let result = match self.write_once(buf) {
            Err(_) => {
                self.file = None;
                self.write_once(buf)
            }
            written => written,
        };
        if let Err(error) = &result
            && self.lost_error.is_none()
        {
            self.lost_error = Some(error.to_string());
        }
        result
    }

    fn write_once(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.rotate_if_needed(buf.len() as u64)?;
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("log file is not open"))?;
        if let Some(error) = self.lost_error.take()
            && let Err(write_error) = writeln!(
                file,
                "shepr: file logging resumed; log lines were lost after an I/O error: {error}"
            )
        {
            self.lost_error = Some(error);
            return Err(write_error);
        }
        file.write(buf)
    }

    fn flush_with_recovery(&mut self) {
        if let Some(file) = self.file.as_mut()
            && file.flush().is_err()
        {
            // Reopened by the next write.
            self.file = None;
        }
    }

    fn rotate_if_needed(&mut self, incoming_len: u64) -> io::Result<()> {
        let size = self.sync_with_path()?;
        if !self.exceeds_limit(size, incoming_len) {
            return Ok(());
        }

        // Several processes can cross the limit together. Rotation happens
        // under an exclusive lock on the current file, and whoever gets the
        // lock second finds the path already pointing at a fresh file.
        let Some(file) = self.file.as_ref() else {
            return Ok(());
        };
        let lock = FileLock::exclusive(file)?;
        let rotate = match fs::metadata(&self.path) {
            Ok(meta) => self.is_current_file(&meta) && self.exceeds_limit(meta.len(), incoming_len),
            Err(err) if err.kind() == io::ErrorKind::NotFound => false,
            Err(err) => return Err(err),
        };
        if rotate {
            self.rotate_files()?;
        }
        drop(lock);
        self.open_current_file()
    }

    fn exceeds_limit(&self, size: u64, incoming_len: u64) -> bool {
        // An empty file is never rotated, or a single message above the limit
        // would rotate forever.
        self.max_bytes != 0 && size > 0 && size.saturating_add(incoming_len) > self.max_bytes
    }

    /// Make sure the open file is the one at `path` (reopening if another
    /// process rotated or removed it) and return its current size.
    fn sync_with_path(&mut self) -> io::Result<u64> {
        match fs::metadata(&self.path) {
            Ok(meta) if self.is_current_file(&meta) => return Ok(meta.len()),
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        self.open_current_file()?;
        match self.file.as_ref() {
            Some(file) => Ok(file.metadata()?.len()),
            None => Ok(0),
        }
    }

    fn is_current_file(&self, path_meta: &fs::Metadata) -> bool {
        use std::os::unix::fs::MetadataExt;

        self.file
            .as_ref()
            .and_then(|file| file.metadata().ok())
            .is_some_and(|open| open.dev() == path_meta.dev() && open.ino() == path_meta.ino())
    }

    fn open_current_file(&mut self) -> io::Result<()> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(LOG_FILE_MODE)
            .open(&self.path)?;
        // `mode` only applies to a file this call creates; tighten one left
        // behind by an older build that created logs world-readable.
        if let Ok(meta) = file.metadata()
            && meta.permissions().mode() & 0o077 != 0
        {
            let _ = file.set_permissions(fs::Permissions::from_mode(LOG_FILE_MODE));
        }
        self.file = Some(file);
        Ok(())
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
            if !source.exists() {
                continue;
            }
            fs::rename(source, target)?;
        }

        Ok(())
    }
}

/// An exclusive `flock(2)` on an open file, released on drop.
struct FileLock<'a> {
    file: &'a File,
}

impl<'a> FileLock<'a> {
    fn exclusive(file: &'a File) -> io::Result<Self> {
        use std::os::fd::AsRawFd;

        loop {
            // SAFETY: flock(2) on a descriptor owned by `file`, which outlives
            // the returned guard; it touches no memory of this process.
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
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
    let file_name = path
        .file_name()
        .map(|name| {
            let mut name = name.to_os_string();
            name.push(&suffix);
            name
        })
        .unwrap_or_else(|| suffix.clone().into());
    path.with_file_name(file_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A log path in a scratch directory kept until the test process exits;
    /// each test removes the directory itself.
    fn temp_log_path(name: &str) -> PathBuf {
        shepr_test_support::ScratchDir::new(name)
            .keep_until_exit()
            .join("shepr.log")
    }

    #[test]
    fn help_names_only_the_logs_that_are_written() {
        let summary = log_paths_summary(Path::new("/data"));
        assert!(summary.starts_with("/data/shepr-server.log"), "{summary}");
        assert!(summary.contains(CLIENT_LOG_FILE), "{summary}");
        assert!(!summary.contains("/shepr.log"), "{summary}");
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
        fs::create_dir_all(path.parent().expect("test precondition")).expect("test precondition");
        fs::write(&path, "current").expect("test precondition");
        fs::write(rotated_log_path(&path, 1), "older").expect("test precondition");

        let state = RotatingFileState {
            path: path.clone(),
            max_bytes: 128,
            retained_files: 2,
            file: None,
            lost_error: None,
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
        assert!(!path.exists());

        let _ = fs::remove_dir_all(path.parent().expect("test precondition"));
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
        assert!(!rotated_log_path(&path, 1).exists());

        let _ = fs::remove_dir_all(dir);
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
        let _ = fs::remove_dir_all(&dir);

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
        let _ = fs::remove_dir_all(&dir);

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
        let _ = fs::remove_dir_all(&dir);

        let contents = contents.expect("log recreated");
        assert!(
            contents.starts_with("shepr: file logging resumed; log lines were lost"),
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

        // A log left world-readable by an older build is tightened on open.
        let legacy = dir.join("legacy.log");
        fs::write(&legacy, "old").expect("legacy log");
        fs::set_permissions(&legacy, fs::Permissions::from_mode(0o644)).expect("legacy mode");
        let _reopened = RotatingFileMakeWriter::new(&dir, "legacy.log", 0, 0).expect("writer");
        let legacy_mode = fs::metadata(&legacy).expect("log").permissions().mode() & 0o777;

        let _ = fs::remove_dir_all(&dir);

        assert_eq!(created_mode, 0o600);
        assert_eq!(legacy_mode, 0o600);
    }
}
