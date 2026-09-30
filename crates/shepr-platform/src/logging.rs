//! Platform log levels: debug is routine diagnostic detail; info records
//! lifecycle and successful state changes; warn records recoverable failures
//! or degraded operation; error records operations that failed or state that
//! could not be preserved.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriter;

/// The filter every file log starts with when `SHEPR_LOG` is unset or empty.
const DEFAULT_LOG_FILTER: &str = "shepr=info";

/// The validated filter for one process's file logger.
pub struct FileLoggingConfig {
    filter: EnvFilter,
}

/// File logging could not be enabled. The caller supplies any operator-facing
/// explanation; `reason` is the underlying filesystem error detail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileLoggingUnavailable {
    pub path: PathBuf,
    pub reason: String,
}

/// Outcome of installing file logging. `unavailable` is present when setup
/// failed without preventing the process from starting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileLoggingOutcome {
    pub unavailable: Option<FileLoggingUnavailable>,
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
/// the default level. A log file that cannot be opened is returned as a
/// [`FileLoggingOutcome`] containing a [`FileLoggingUnavailable`] value, and
/// the process runs without file logging.
/// An already-installed global logger also fails initialization because this
/// call cannot install its file writer.
pub fn init_file_logging(dir: &Path, file_name: &str) -> io::Result<FileLoggingOutcome> {
    init_file_logging_with_config(dir, file_name, FileLoggingConfig::from_environment()?)
}

/// Installs the file logger with a filter already validated for this process.
pub fn init_file_logging_with_config(
    dir: &Path,
    file_name: &str,
    config: FileLoggingConfig,
) -> io::Result<FileLoggingOutcome> {
    let filter = config.filter;

    let make_writer = match RotatingFileMakeWriter::new(
        dir,
        file_name,
        super::limits::DEFAULT_MAX_LOG_BYTES,
        super::limits::DEFAULT_RETAINED_LOG_FILES,
    ) {
        Ok(make_writer) => make_writer,
        Err(error) => {
            return Ok(FileLoggingOutcome {
                unavailable: Some(FileLoggingUnavailable {
                    path: dir.join(file_name),
                    reason: error.to_string(),
                }),
            });
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
            error = %error,
            "file logging not installed: a logger is already set"
        );
        return Err(io::Error::other(format!(
            "file logging could not be initialized because a logger is already set: {error}"
        )));
    }
    Ok(FileLoggingOutcome::default())
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
pub fn init_client_file_logging(
    dir: &Path,
    config: FileLoggingConfig,
) -> io::Result<FileLoggingOutcome> {
    init_file_logging_with_config(dir, CLIENT_LOG_FILE, config)
}

/// The log files `--help` names: the only two any process writes.
pub fn help_log_paths_summary(dir: &Path) -> String {
    format!(
        "{} (and {CLIENT_LOG_FILE} beside it)",
        dir.join(SERVER_LOG_FILE).display()
    )
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
            lost_reason: None,
            current_file: None,
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
        let mut state = self.lock_state();
        let pending_reason = state.lost_reason.take();
        let result = state.write_once(buf, pending_reason.as_deref());
        match result {
            Ok(written) => Ok(written),
            Err(error) => {
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
/// The open descriptor is reused; every write checks the path inode so another
/// process can rotate without leaving this writer appending to an old generation.
struct RotatingFileState {
    path: PathBuf,
    max_bytes: u64,
    retained_files: usize,
    /// The reason for an ongoing logging gap, reported once writing works
    /// again.
    lost_reason: Option<String>,
    current_file: Option<OpenLogFile>,
}

struct OpenLogFile {
    file: File,
    dev: u64,
    ino: u64,
}

/// Log files hold pane activity and error details; keep them private to the
/// user like the rest of the data directory's state.
// limits-exempt: this is the logger's POSIX file mode, kept beside its open call.
const LOG_FILE_MODE: u32 = 0o600;

impl RotatingFileState {
    /// Write one chunk. The local state mutex protects the cached descriptor;
    /// file-level flocks coordinate writes and rotation across processes.
    fn write_once(&mut self, buf: &[u8], pending_reason: Option<&str>) -> io::Result<usize> {
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
            if self.current_file.is_none() {
                self.open_current_file()?;
            }
            let current = self
                .current_file
                .as_ref()
                .ok_or_else(|| io::Error::other("rotating log writer has no open current file"))?;
            let lock = FileLock::shared(&current.file)?;
            // Check every record so a second process's rotation cannot leave
            // this cached descriptor appending to the previous generation.
            let path_metadata = match fs::metadata(&self.path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    drop(lock);
                    self.current_file = None;
                    continue;
                }
                Err(error) => return Err(error),
            };
            if !current.matches(&path_metadata) {
                drop(lock);
                self.current_file = None;
                continue;
            }
            let size = path_metadata.len();
            if self.exceeds_limit(size, incoming_len) {
                drop(lock);
                self.rotate_if_needed(incoming_len)?;
                continue;
            }
            // Do not retry this operation: write_all can append a prefix before
            // returning an error, and replaying the whole buffer would duplicate it.
            let mut append = &current.file;
            append.write_all(write_buf)?;
            drop(lock);
            return Ok(buf.len());
        }
    }

    fn rotate_if_needed(&mut self, incoming_len: u64) -> io::Result<()> {
        loop {
            if self.current_file.is_none() {
                self.open_current_file()?;
            }
            let current = self
                .current_file
                .as_ref()
                .ok_or_else(|| io::Error::other("rotating log writer has no open current file"))?;
            let lock = FileLock::exclusive(&current.file)?;
            let path_metadata = match fs::metadata(&self.path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    drop(lock);
                    self.current_file = None;
                    continue;
                }
                Err(error) => return Err(error),
            };
            if !current.matches(&path_metadata) {
                drop(lock);
                self.current_file = None;
                continue;
            }
            if self.exceeds_limit(path_metadata.len(), incoming_len) {
                self.rotate_files()?;
                drop(lock);
                self.current_file = None;
                return Ok(());
            }
            drop(lock);
            return Ok(());
        }
    }

    fn exceeds_limit(&self, size: u64, incoming_len: u64) -> bool {
        // An empty file is never rotated, or a single message above the limit
        // would rotate forever.
        self.max_bytes != 0 && size > 0 && size.saturating_add(incoming_len) > self.max_bytes
    }

    fn open_current_file(&mut self) -> io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(LOG_FILE_MODE)
            .open(&self.path)?;
        let metadata = file.metadata()?;
        self.current_file = Some(OpenLogFile {
            file,
            dev: metadata.dev(),
            ino: metadata.ino(),
        });
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

impl OpenLogFile {
    fn matches(&self, metadata: &fs::Metadata) -> bool {
        self.dev == metadata.dev() && self.ino == metadata.ino()
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
        let summary = help_log_paths_summary(Path::new("/data"));
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
            current_file: None,
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
