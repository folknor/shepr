//! One server per data directory.
//!
//! The socket check (`prepare_socket_path`) only stops two servers on the same
//! socket. The session files are chosen differently: the data directory
//! follows the session name and never the socket override, so a second server
//! started with `SHEPR_SOCKET_PATH` pointing elsewhere passes the socket check
//! and would otherwise restore the same layout, resume the same native agent
//! conversations a second time, and race the first server's autosaves.
//!
//! Deriving the data directory from the socket instead was rejected: it would
//! move an override user's session files to wherever their socket lives (often
//! a temporary directory), and a restart without the override would no longer
//! find them. Instead the server that first reads or writes a data directory's
//! session files takes an exclusive `flock` on `session.lock` there and holds
//! it until its final shutdown save. The kernel drops the lock when the process
//! dies, so a crash never leaves a stale lock behind, and the descriptor is
//! close-on-exec (as every std-opened file is), so pane children never inherit
//! it.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

pub(super) const LOCK_FILE_NAME: &str = "session.lock";

struct HeldLock {
    directory: PathBuf,
    // Dropping the file closes the descriptor, which releases the lock.
    _file: File,
}

/// Locks this process holds, one per data directory. A process only ever
/// owns its own session's directory in production; tests open many.
static HELD: Mutex<Vec<HeldLock>> = Mutex::new(Vec::new());

fn held() -> std::sync::MutexGuard<'static, Vec<HeldLock>> {
    // The list holds plain handles with no invariant a panic could break.
    HELD.lock().unwrap_or_else(PoisonError::into_inner)
}

fn lock_key(directory: &Path) -> PathBuf {
    std::fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf())
}

/// Whether a failed claim means another process owns the directory, as
/// opposed to the lock file being unusable.
pub(super) fn is_owned_elsewhere(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::ResourceBusy
}

/// Claims `directory`'s session files for this process. Idempotent while the
/// claim is held. Fails with `ResourceBusy` when another process holds it;
/// any other error means the lock file itself could not be used.
pub(crate) fn claim(directory: &Path) -> io::Result<()> {
    std::fs::create_dir_all(directory)?;
    let key = lock_key(directory);
    let mut held = held();
    if held.iter().any(|lock| lock.directory == key) {
        return Ok(());
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(key.join(LOCK_FILE_NAME))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!(
                    "another shepr server owns the session files in {}",
                    key.display()
                ),
            ));
        }
        Err(TryLockError::Error(err)) => return Err(err),
    }
    held.push(HeldLock {
        directory: key,
        _file: file,
    });
    Ok(())
}

/// Gives up this process's claim on `directory`, if it has one.
pub(super) fn release(directory: &Path) {
    let key = lock_key(directory);
    held().retain(|lock| lock.directory != key);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A data directory that does not exist yet, in a scratch directory kept
    /// until the test process exits.
    fn temp_directory(name: &str) -> PathBuf {
        crate::test_support::ScratchDir::new(name)
            .keep_until_exit()
            .join("data")
    }

    /// A lock taken through a separate open file description conflicts with
    /// ours exactly as another process's would.
    fn foreign_lock(directory: &Path) -> File {
        std::fs::create_dir_all(directory).expect("test precondition");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join(LOCK_FILE_NAME))
            .expect("test precondition");
        file.try_lock().expect("test precondition");
        file
    }

    #[test]
    fn claim_is_idempotent_and_excludes_other_owners() {
        let directory = temp_directory("idempotent");
        claim(&directory).expect("first claim");
        claim(&directory).expect("repeat claim by the owner");
        let other = OpenOptions::new()
            .read(true)
            .open(directory.join(LOCK_FILE_NAME))
            .expect("test precondition");
        assert!(matches!(other.try_lock(), Err(TryLockError::WouldBlock)));

        release(&directory);
        other.try_lock().expect("released claim frees the lock");
        drop(other);
        std::fs::remove_dir_all(&directory).expect("test precondition");
    }

    #[test]
    fn claim_fails_as_busy_while_another_owner_holds_the_directory() {
        let directory = temp_directory("busy");
        let foreign = foreign_lock(&directory);

        let err = claim(&directory).expect_err("directory is owned elsewhere");
        assert!(is_owned_elsewhere(&err), "{err}");

        drop(foreign);
        claim(&directory).expect("claim succeeds once the other owner is gone");
        release(&directory);
        std::fs::remove_dir_all(&directory).expect("test precondition");
    }

    #[test]
    fn lock_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let directory = temp_directory("private");
        claim(&directory).expect("test precondition");
        let mode = std::fs::metadata(directory.join(LOCK_FILE_NAME))
            .expect("test precondition")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        release(&directory);
        std::fs::remove_dir_all(&directory).expect("test precondition");
    }
}
