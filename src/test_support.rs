//! Test isolation shared by every unit test in the crate.
//!
//! Two rules, each implemented once, here:
//!
//! - Scratch files live in a [`ScratchDir`]: a fresh directory per test under
//!   one root that this test process creates and owns, removed when the test
//!   ends (and the root when the process exits). Never a fixed path shared
//!   between tests or runs under `/tmp` or `/var/tmp`, and never anything
//!   under the real `$HOME`.
//! - A test that changes the process environment, or reads a variable (or the
//!   explicit-session flag) that another test changes, holds an
//!   [`IsolatedEnv`] for its whole body. There is one lock for the whole
//!   crate, so these tests exclude each other whichever module they live in.
//!   The guard also points `HOME` and `XDG_RUNTIME_DIR` at scratch and clears
//!   the other XDG base directories and every inherited `SHEPR_*` variable,
//!   so nothing under test
//!   can reach the user's real config, state or agent directories, or the live
//!   shepr server a test run was started from. It restores the whole
//!   environment when dropped, including on panic.
//!
//! Prefer passing a value in over setting an environment variable: code that
//! takes the path or setting as an argument needs neither the lock nor the
//! guard.

use std::ffi::{OsStr, OsString};
use std::ops::Deref;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

static SCRATCH_ROOT: OnceLock<PathBuf> = OnceLock::new();
static SCRATCH_ROOT_OWNER: AtomicU32 = AtomicU32::new(0);
static NEXT_SCRATCH: AtomicUsize = AtomicUsize::new(0);
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// The XDG base directories cleared by [`IsolatedEnv`], so paths fall back to
/// the scratch `HOME`.
const XDG_BASE_DIR_VARS: [&str; 4] = [
    "XDG_CONFIG_HOME",
    "XDG_STATE_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
];

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

/// The root every [`ScratchDir`] of this test process lives under, created on
/// first use.
///
/// It sits under the system temp directory rather than `target/`: tests bind
/// Unix sockets inside scratch directories, and a socket path has to fit in
/// `sun_path` (108 bytes), which a deep checkout path would eat. The root is
/// resolved once, so a test that changes `TMPDIR` does not move later scratch
/// directories.
fn scratch_root() -> &'static Path {
    SCRATCH_ROOT.get_or_init(|| {
        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("shepr-test-{pid}"));
        // A directory with this name can only be left over from an earlier run
        // with the same pid that was killed before its exit cleanup ran.
        let _ = std::fs::remove_dir_all(&root);
        create_private_dir(&root).expect("create the test scratch root");
        SCRATCH_ROOT_OWNER.store(pid, Ordering::Release);
        // SAFETY: atexit(3) only records a function pointer. The handler is a
        // plain `extern "C"` function that cannot unwind: it reads statics
        // and ignores every error.
        unsafe { libc::atexit(remove_scratch_root) };
        root
    })
}

extern "C" fn remove_scratch_root() {
    // A forked child that calls exit(3) runs this too; only the process that
    // created the root removes it.
    if SCRATCH_ROOT_OWNER.load(Ordering::Acquire) != std::process::id() {
        return;
    }
    if let Some(root) = SCRATCH_ROOT.get() {
        let _ = std::fs::remove_dir_all(root);
    }
}

/// A fresh, private (0700) directory for one test, removed when dropped.
///
/// `label` only makes the directory recognisable; uniqueness comes from a
/// per-process counter. Keep labels short: sockets are bound in here.
pub(crate) struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    pub(crate) fn new(label: &str) -> Self {
        let index = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
        let path = scratch_root().join(format!("{index}-{label}"));
        create_private_dir(&path).expect("create a test scratch directory");
        Self { path }
    }

    /// Makes private scratch under a chosen filesystem for tests that need to
    /// exercise a particular mount's filesystem features.
    pub(crate) fn new_in(base: &Path, label: &str) -> Self {
        let index = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!("shepr-test-{}-{index}-{label}", std::process::id()));
        create_private_dir(&path).expect("create a test scratch directory");
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Gives up per-test cleanup for a directory that must outlive this
    /// guard, such as one a returned fixture keeps using. It is still removed
    /// with the scratch root when the test process exits.
    pub(crate) fn keep_until_exit(self) -> PathBuf {
        let mut this = std::mem::ManuallyDrop::new(self);
        std::mem::take(&mut this.path)
    }
}

impl Deref for ScratchDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for ScratchDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Exclusive, restorable access to the process environment for one test.
///
/// Holding it serializes the test against every other test that holds one,
/// crate-wide. On creation it snapshots the environment, sets `HOME` to a
/// fresh scratch directory, removes the XDG base directory variables and every
/// `SHEPR_*` variable. On drop it puts the snapshot back exactly.
///
/// Change variables through [`IsolatedEnv::set`] and [`IsolatedEnv::remove`]:
/// borrowing the guard is what proves the lock is held.
pub(crate) struct IsolatedEnv {
    saved: Vec<(OsString, OsString)>,
    scratch: ScratchDir,
    // Declared last so it is released after the environment is restored and
    // the scratch directory removed.
    _lock: MutexGuard<'static, ()>,
}

impl IsolatedEnv {
    pub(crate) fn new() -> Self {
        // A test that panicked while holding the lock still restored the
        // environment on unwind, so a poisoned lock carries no bad state.
        let lock = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let saved: Vec<_> = std::env::vars_os().collect();
        let scratch = ScratchDir::new("env");
        let home = scratch.join("home");
        create_private_dir(&home).expect("create the scratch HOME");
        let env = Self {
            saved,
            scratch,
            _lock: lock,
        };
        env.set("HOME", &home);
        for key in XDG_BASE_DIR_VARS {
            env.remove(key);
        }
        env.set("XDG_RUNTIME_DIR", env.path().join("runtime"));
        let inherited_shepr: Vec<OsString> = env
            .saved
            .iter()
            .map(|(key, _)| key)
            .filter(|key| key.to_str().is_some_and(|key| key.starts_with("SHEPR_")))
            .cloned()
            .collect();
        for key in inherited_shepr {
            env.remove(key);
        }
        env
    }

    /// This test's scratch directory. `HOME` is its `home` subdirectory.
    pub(crate) fn path(&self) -> &Path {
        self.scratch.path()
    }

    /// The scratch directory `HOME` points at.
    pub(crate) fn home(&self) -> PathBuf {
        self.scratch.join("home")
    }

    pub(crate) fn set(&self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) {
        // SAFETY: set_var is unsafe because another thread may read the
        // environment at the same time. Every test that changes the
        // environment does so through an `IsolatedEnv`, and `&self` proves
        // this call happens while the crate-wide environment lock is held.
        unsafe { std::env::set_var(key, value) };
    }

    pub(crate) fn remove(&self, key: impl AsRef<OsStr>) {
        // SAFETY: as in `set`; `&self` proves the environment lock is held.
        unsafe { std::env::remove_var(key) };
    }
}

impl Drop for IsolatedEnv {
    fn drop(&mut self) {
        let current: Vec<OsString> = std::env::vars_os().map(|(key, _)| key).collect();
        for key in current {
            if !self.saved.iter().any(|(saved, _)| *saved == key) {
                self.remove(&key);
            }
        }
        for (key, value) in &self.saved {
            if std::env::var_os(key).as_ref() != Some(value) {
                self.set(key, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_dirs_are_private_distinct_and_removed_on_drop() {
        use std::os::unix::fs::PermissionsExt;

        let first = ScratchDir::new("a");
        let second = ScratchDir::new("a");
        assert_ne!(first.path(), second.path());
        assert!(first.starts_with(scratch_root()));
        let mode = std::fs::metadata(first.path())
            .expect("test precondition")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);

        let path = first.to_path_buf();
        drop(first);
        assert!(!path.exists());
    }

    #[test]
    fn isolated_env_points_home_at_scratch_and_restores_on_drop() {
        const PROBE: &str = "SHEPR_TEST_SUPPORT_PROBE";
        let env = IsolatedEnv::new();
        assert_eq!(std::env::var_os("HOME"), Some(env.home().into_os_string()));
        assert!(std::env::var_os("XDG_CONFIG_HOME").is_none());
        let paths = crate::config::AppPaths::resolve().expect("isolated directories resolve");
        assert!(paths.config_dir().starts_with(env.path()));
        assert!(paths.state_dir().starts_with(env.path()));
        assert!(paths.runtime_dir().starts_with(env.path()));
        env.set(PROBE, "set");
        let scratch = env.path().to_path_buf();
        drop(env);

        assert!(!scratch.exists());
        // No test sets this variable other than through a guard, so it is
        // gone once the guard has restored the snapshot.
        assert!(std::env::var_os(PROBE).is_none());
    }
}
