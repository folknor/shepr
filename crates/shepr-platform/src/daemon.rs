//! What a client needs to start the server daemon and to clean up after a
//! launch that did not succeed: the private runtime directory, the boot log the
//! daemon's stderr goes to, and the guard that kills a half-started daemon.

use std::fs::{self, File};
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;
use std::process::{Child, ExitStatus};

/// Creates `path` and any missing parents owner-only. An existing directory is
/// left as it is, whatever its mode: this is a path-creation convenience, not
/// a private-directory assertion. Trust-sensitive callers validate the
/// directory they use; SSH runtime storage requires the exact private mode,
/// and socket listeners authenticate peers independently.
pub fn create_private_directory_all(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(super::limits::PRIVATE_DIRECTORY_MODE)
        .create(path)
}

/// Creates shepr's own runtime directory `path` (and missing parents)
/// owner-only, and tightens an existing one to the private mode when this user
/// owns it and it is a real directory, not a symlink: whatever created it, the
/// directory shepr's sockets live in is always private, and tightening only
/// ever removes access. A directory
/// someone else owns, or a symlink, is left as it is for the caller's own check
/// to refuse.
pub fn create_private_runtime_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    create_private_directory_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir()
        && metadata.uid() == super::effective_uid()
        && metadata.mode() & super::limits::PERMISSION_BITS != super::limits::PRIVATE_DIRECTORY_MODE
    {
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(super::limits::PRIVATE_DIRECTORY_MODE),
        )?;
    }
    Ok(())
}

/// Opens the file a launched daemon's stderr goes to, emptied. The server
/// keeps it as stderr through startup and points stderr at `/dev/null` at
/// readiness once its own log is running; one whose log could not be opened
/// keeps it, writing to it after readiness only a capped number of panic
/// reports and its exit error.
///
/// A regular file rather than a pipe, so a detached daemon can never block on
/// a reader that went away. It is opened owner-only without following a
/// symlink, and refused unless it is a regular file this user owns, so a
/// planted link or foreign file in the runtime directory is never written
/// through. It is emptied only after those checks. It is opened for append, so
/// the launcher can empty or inspect it through its own handle while the
/// daemon keeps writing at the end.
pub fn open_boot_log(path: &Path) -> io::Result<File> {
    let file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .truncate(false)
        .mode(super::limits::PRIVATE_FILE_MODE)
        // O_NONBLOCK keeps a FIFO planted at the path from stalling the open;
        // it is refused as a non-regular file just below, and the flag does
        // nothing for the regular file the daemon writes.
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| super::private_file::PrivateFile::normalize_open_error(path, error))?;
    let metadata = file.metadata()?;
    super::private_file::PrivateFile::require_owned_regular(
        path,
        &metadata,
        super::effective_uid(),
    )?;
    file.set_permissions(fs::Permissions::from_mode(super::limits::PRIVATE_FILE_MODE))?;
    file.set_len(0)?;
    Ok(file)
}

/// The last part of a boot log, for a failure message: at most
/// `BOOT_LOG_TAIL_BYTES`, decoded lossily and trimmed. Like
/// [`open_boot_log`], it follows no link and refuses files not owned by this
/// user.
pub fn read_boot_log_tail(path: &Path) -> io::Result<String> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| super::private_file::PrivateFile::normalize_open_error(path, error))?;
    let metadata = file.metadata()?;
    super::private_file::PrivateFile::require_owned_regular(
        path,
        &metadata,
        super::effective_uid(),
    )?;
    let start = metadata
        .len()
        .saturating_sub(super::limits::BOOT_LOG_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(super::limits::BOOT_LOG_TAIL_BYTES)
        .read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim();
    Ok(if start > 0 {
        format!("[earlier output omitted]\n{text}")
    } else {
        text.to_owned()
    })
}

/// A daemon this process just started, killed and reaped along with its whole
/// process group unless the launch is explicitly resolved.
///
/// The daemon spawn path calls `setsid`, so the child leads a group of its own
/// and one signal reaches everything it started. Holding the child here means
/// an early return, a failed probe or a timeout cannot orphan a half-started
/// daemon: [`Drop`] kills it. Only [`SpawnedDaemon::disarm`], called after the
/// daemon proved itself, keeps it running.
///
/// The group is signalled only while the leader is unreaped, which is what
/// keeps its id from naming another process. A leader [`try_wait`] has seen
/// exit was reaped by that call, so nothing is signalled afterwards.
///
/// [`try_wait`]: SpawnedDaemon::try_wait
pub struct SpawnedDaemon {
    child: Option<Child>,
}

impl SpawnedDaemon {
    pub fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    /// The daemon's process id while it has not been seen to exit.
    pub fn process_id(&self) -> Option<crate::Pid> {
        self.child
            .as_ref()
            .and_then(|child| crate::Pid::new(child.id()))
    }

    /// Polls the child without waiting and reports its exit status once. A
    /// later call returns `None`, just as a call made while it is still
    /// running does; the caller that needs the status again must retain it.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let status = child.try_wait()?;
        if status.is_some() {
            self.child = None;
        }
        Ok(status)
    }

    /// Resolves the launch: the daemon stays running and is no longer this
    /// guard's to kill. A thread waits for it so it does not linger as a
    /// zombie while this process lives.
    pub fn disarm(mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let pid = child.id();
        let reaper = std::thread::Builder::new()
            // Linux exposes at most 15 bytes through `pthread_setname_np`.
            .name("daemon-reaper".into())
            .spawn(move || {
                if let Err(error) = child.wait() {
                    tracing::debug!(pid, %error, "could not wait for the server daemon");
                }
            });
        if let Err(error) = reaper {
            crate::structured_log!(WARN, event = daemon.reaper_start, outcome = "error", pid, %error, "could not start the server daemon reaper");
        }
    }
}

impl Drop for SpawnedDaemon {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let pid = child.id();
        if let Err(error) = crate::Pid::new(pid)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid daemon pid"))
            .and_then(|pid| kill_process_group(crate::Pgid::led_by(pid)))
        {
            crate::structured_log!(WARN, event = daemon.group_kill, outcome = "error", pid, %error, "could not kill the server daemon's process group");
        }
        // A child that does not lead its own group has none to signal above.
        // Killing it directly is a no-op error when it already exited.
        if let Err(error) = child.kill()
            && error.kind() != io::ErrorKind::InvalidInput
        {
            tracing::debug!(pid, %error, "could not kill the server daemon");
        }
        if let Err(error) = child.wait() {
            crate::structured_log!(WARN, event = daemon.child_reap, outcome = "error", pid, %error, "could not reap the killed server daemon");
        }
    }
}

/// SIGKILLs the group of an unreaped process-group leader. A group that
/// no longer exists is success.
fn kill_process_group(group: crate::Pgid) -> io::Result<()> {
    if group.get() <= 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid process group",
        ));
    }
    let group = group.as_pid_t();
    // SAFETY: kill(2) with a negative pid signals the group `group`; it reads
    // and writes no memory of ours. The caller holds the unreaped leader, so
    // the id cannot have been reused for another process.
    let result = unsafe { libc::kill(-group, libc::SIGKILL) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_support::fixture::{self, Held, Step};
    use std::os::unix::fs::MetadataExt as _;
    use std::time::{Duration, Instant};

    fn group_is_gone(group: u32) -> bool {
        let group = libc::pid_t::try_from(group).expect("pid fits");
        // SAFETY: kill(2) with signal zero only probes the group.
        let result = unsafe { libc::kill(-group, 0) };
        result != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    fn spawn_daemon_with_grandchild() -> (SpawnedDaemon, u32) {
        let mut command = fixture::command(&[
            Step::Spawn {
                argv0: "shepr-grandchild".into(),
                sleep: Duration::from_secs(60),
                held: Held::Nothing,
            },
            Step::Sleep(Duration::from_secs(60)),
        ]);
        crate::detach_server_daemon_command(&mut command);
        let child = command.spawn().expect("spawn the stand-in daemon");
        let pid = child.id();
        (SpawnedDaemon::new(child), pid)
    }

    #[test]
    fn dropping_the_guard_kills_the_whole_process_group() {
        let (guard, pid) = spawn_daemon_with_grandchild();
        // Let the leader start its own child before the group is killed.
        std::thread::sleep(Duration::from_millis(200));
        drop(guard);

        let deadline = Instant::now() + Duration::from_secs(5);
        while !group_is_gone(pid) {
            assert!(
                Instant::now() < deadline,
                "the daemon's process group survived the guard"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_disarmed_guard_leaves_the_daemon_running() {
        let (guard, pid) = spawn_daemon_with_grandchild();
        guard.disarm();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!group_is_gone(pid), "a disarmed daemon must keep running");

        let group = libc::pid_t::try_from(pid).expect("pid fits");
        // SAFETY: the daemon leads the group and is still unreaped (the reaper
        // thread waits on it), so the id names it and its child alone.
        let result = unsafe { libc::kill(-group, libc::SIGKILL) };
        assert_eq!(result, 0, "clean up the stand-in daemon");
    }

    #[test]
    fn try_wait_reports_an_exit_once_and_stops_signalling() {
        let mut command = fixture::command(&[Step::Exit(7)]);
        crate::detach_server_daemon_command(&mut command);
        let mut guard = SpawnedDaemon::new(command.spawn().expect("spawn the stand-in daemon"));
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = guard.try_wait().expect("poll the daemon") {
                break status;
            }
            assert!(Instant::now() < deadline, "the daemon never exited");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(7));
        assert_eq!(guard.process_id(), None);
        assert!(guard.try_wait().expect("poll again").is_none());
    }

    #[test]
    fn boot_log_is_private_emptied_and_never_followed_through_a_link() {
        let dir = shepr_test_support::ScratchDir::new("boot-log");
        let path = dir.join("boot.log");
        fs::write(&path, b"old output").expect("test precondition");

        let file = open_boot_log(&path).expect("open the boot log");
        drop(file);
        let metadata = fs::metadata(&path).expect("boot log exists");
        assert_eq!(metadata.len(), 0, "a boot log starts empty");
        assert_eq!(
            metadata.mode() & crate::limits::PERMISSION_BITS,
            crate::limits::PRIVATE_FILE_MODE
        );

        let target = dir.join("target");
        fs::write(&target, b"keep").expect("test precondition");
        let link = dir.join("linked.log");
        std::os::unix::fs::symlink(&target, &link).expect("plant a link");
        open_boot_log(&link).expect_err("a symlinked boot log is refused");
        read_boot_log_tail(&link).expect_err("a symlinked boot log is not read");
        assert_eq!(fs::read(&target).expect("target kept"), b"keep");
    }

    #[test]
    fn boot_log_tail_is_bounded_and_marks_what_it_dropped() {
        let dir = shepr_test_support::ScratchDir::new("boot-log-tail");
        let path = dir.join("boot.log");
        let tail_limit =
            usize::try_from(crate::limits::BOOT_LOG_TAIL_BYTES).expect("the limit fits");
        let mut text = "x".repeat(tail_limit * 2);
        text.push_str("\nfinal line\n");
        fs::write(&path, &text).expect("test precondition");

        let tail = read_boot_log_tail(&path).expect("read the tail");
        assert!(tail.starts_with("[earlier output omitted]"), "{tail}");
        assert!(tail.ends_with("final line"), "{tail}");

        fs::write(&path, b"short\n").expect("test precondition");
        assert_eq!(read_boot_log_tail(&path).expect("read the tail"), "short");
    }
}
