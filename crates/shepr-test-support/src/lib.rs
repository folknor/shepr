//! Test isolation shared by every unit test in the crate.
//!
//! Two rules, each implemented once, here:
//!
//! - Scratch files live in a [`ScratchDir`]: a fresh private directory per
//!   test. `new` puts it under one root created by the test process; `new_in`
//!   puts it on the chosen filesystem. Dropped directories are removed at the
//!   end of the test, and directories kept until exit are removed by process
//!   cleanup. Never a fixed or shared path under `/tmp` or `/var/tmp`, and
//!   never anything under the real `$HOME`.
//! - A test that changes the process environment, or reads a variable (or the
//!   explicit-session flag) that another test changes, holds an
//!   [`IsolatedEnv`] for its whole body. These tests serialize with one another
//!   when they run in the same test process, whichever module they live in.
//!   The guard clears every variable in shepr's registry
//!   (`shepr_core::env::EnvVar`), so a variable added there is isolated with
//!   no change here, then points `HOME` and `XDG_RUNTIME_DIR` at scratch. It
//!   also clears every other inherited `SHEPR_*` variable and the XDG base
//!   directories shepr does not read but the tools tests spawn do. Nothing
//!   under test can reach the user's real config, state or agent directories,
//!   or the live shepr server a test run was started from. It restores the
//!   whole environment when dropped, including on panic.
//!
//! Prefer passing a value in over setting an environment variable: code that
//! takes the path or setting as an argument needs neither the lock nor the
//! guard.
//!
//! This crate is where tests touch the process environment directly: the
//! guard's snapshot, writes and restore, and [`IsolatedEnv::get`] for a test
//! asserting what a variable holds. Each such site carries an `#[expect]`.

use std::ffi::{OsStr, OsString};
use std::ops::Deref;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use shepr_core::env::EnvVar;

static SCRATCH_ROOT: OnceLock<PathBuf> = OnceLock::new();
static SCRATCH_ROOT_OWNER: AtomicU32 = AtomicU32::new(0);
static SCRATCH_CLEANUP_REGISTERED: OnceLock<()> = OnceLock::new();
static KEPT_SCRATCH_DIRS: OnceLock<Mutex<Vec<(u32, PathBuf)>>> = OnceLock::new();
static NEXT_SCRATCH: AtomicUsize = AtomicUsize::new(0);
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// XDG base directories no shepr process reads, and so absent from the
/// registry, that the tools tests spawn (Git among them) do read. Cleared so
/// those tools fall back to the scratch `HOME` too.
const FOREIGN_XDG_BASE_DIR_VARS: [&str; 2] = ["XDG_DATA_HOME", "XDG_CACHE_HOME"];

/// The whole process environment, for the guard's snapshot and restore.
#[expect(
    clippy::disallowed_methods,
    reason = "IsolatedEnv snapshots and restores the raw environment; it is the test isolation guard, not a reader"
)]
fn environment_snapshot() -> Vec<(OsString, OsString)> {
    std::env::vars_os().collect()
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

fn ensure_exit_cleanup() {
    let _ = SCRATCH_ROOT_OWNER.compare_exchange(
        0,
        std::process::id(),
        Ordering::AcqRel,
        Ordering::Acquire,
    );
    SCRATCH_CLEANUP_REGISTERED.get_or_init(|| {
        // SAFETY: atexit(3) only records a function pointer. The handler is a
        // plain `extern "C"` function that cannot unwind: it reads statics and
        // ignores every error.
        unsafe { libc::atexit(remove_scratch_root) };
    });
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
    ensure_exit_cleanup();
    SCRATCH_ROOT.get_or_init(|| {
        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("shepr-test-{pid}"));
        // A directory with this name can only be left over from an earlier run
        // with the same pid that was killed before its exit cleanup ran.
        let _ = std::fs::remove_dir_all(&root);
        create_private_dir(&root).expect("create the test scratch root");
        root
    })
}

extern "C" fn remove_scratch_root() {
    let pid = std::process::id();
    // A forked child that calls exit(3) runs this too; only the process that
    // created the root removes it.
    if SCRATCH_ROOT_OWNER.load(Ordering::Acquire) == pid
        && let Some(root) = SCRATCH_ROOT.get()
    {
        let _ = std::fs::remove_dir_all(root);
    }
    remove_kept_scratch_dirs(pid);
}

fn remove_kept_scratch_dirs(owner: u32) {
    if let Some(paths) = KEPT_SCRATCH_DIRS.get() {
        let mut paths = paths.lock().unwrap_or_else(PoisonError::into_inner);
        let mut index = 0;
        while index < paths.len() {
            if paths[index].0 == owner {
                let (_, path) = paths.swap_remove(index);
                let _ = std::fs::remove_dir_all(path);
            } else {
                index += 1;
            }
        }
    }
}

/// A fresh, private (0700) directory for one test, removed when dropped.
///
/// `label` only makes the directory recognisable; uniqueness comes from a
/// per-process counter. Keep labels short: sockets are bound in here.
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    pub fn new(label: &str) -> Self {
        let index = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
        let path = scratch_root().join(format!("{index}-{label}"));
        create_private_dir(&path).expect("create a test scratch directory");
        Self { path }
    }

    /// Makes private scratch under a chosen filesystem for tests that need to
    /// exercise a particular mount's filesystem features.
    pub fn new_in(base: &Path, label: &str) -> Self {
        let index = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!("shepr-test-{}-{index}-{label}", std::process::id()));
        create_private_dir(&path).expect("create a test scratch directory");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Gives up per-test cleanup for a directory that must outlive this
    /// guard, such as one a returned fixture keeps using. It is removed with
    /// this test process's scratch cleanup, even when it was created outside
    /// the default scratch root.
    pub fn keep_until_exit(self) -> PathBuf {
        ensure_exit_cleanup();
        let mut this = std::mem::ManuallyDrop::new(self);
        let path = std::mem::take(&mut this.path);
        let paths = KEPT_SCRATCH_DIRS.get_or_init(|| Mutex::new(Vec::new()));
        paths
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((std::process::id(), path.clone()));
        path
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
/// Holding it serializes the test against other tests in its process that hold
/// one. On creation it snapshots the environment, removes every registered
/// variable, every other `SHEPR_*` variable and the foreign XDG base
/// directories, then sets `HOME` and `XDG_RUNTIME_DIR` to fresh scratch
/// directories. On drop it puts the snapshot back exactly.
///
/// Change variables through [`IsolatedEnv::set`] and [`IsolatedEnv::remove`]:
/// borrowing the guard is what proves the lock is held.
pub struct IsolatedEnv {
    saved: Vec<(OsString, OsString)>,
    scratch: ScratchDir,
    // Declared last so it is released after the environment is restored and
    // the scratch directory removed.
    _lock: MutexGuard<'static, ()>,
}

impl IsolatedEnv {
    pub fn new() -> Self {
        // A test that panicked while holding the lock still restored the
        // environment on unwind, so a poisoned lock carries no bad state.
        let lock = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let saved = environment_snapshot();
        let scratch = ScratchDir::new("env");
        let home = scratch.join("home");
        create_private_dir(&home).expect("create the scratch HOME");
        let env = Self {
            saved,
            scratch,
            _lock: lock,
        };
        create_private_dir(&env.runtime_dir()).expect("create the scratch runtime directory");
        env.isolate();
        env
    }

    /// Clears everything a test must not inherit and points `HOME` and
    /// `XDG_RUNTIME_DIR` at this guard's scratch directories.
    fn isolate(&self) {
        for var in EnvVar::ALL {
            self.remove(var);
        }
        for key in FOREIGN_XDG_BASE_DIR_VARS {
            self.remove(key);
        }
        // Unregistered `SHEPR_*` names are test harness probes and variables
        // only shipped hook assets read; none may leak in from the shell that
        // started the test run either.
        let inherited_shepr: Vec<OsString> = environment_snapshot()
            .into_iter()
            .map(|(key, _)| key)
            .filter(|key| key.to_str().is_some_and(|key| key.starts_with("SHEPR_")))
            .collect();
        for key in inherited_shepr {
            self.remove(key);
        }
        self.set(EnvVar::Home, self.home());
        self.set(EnvVar::XdgRuntimeDir, self.runtime_dir());
    }

    fn runtime_dir(&self) -> PathBuf {
        self.scratch.join("runtime")
    }

    /// This test's scratch directory. `HOME` is its `home` subdirectory.
    pub fn path(&self) -> &Path {
        self.scratch.path()
    }

    /// The scratch directory `HOME` points at.
    pub fn home(&self) -> PathBuf {
        self.scratch.join("home")
    }

    /// Sets a variable. Takes a registry variant (`shepr_core::env::EnvVar`)
    /// or any name, such as a test's own probe variable.
    #[expect(
        clippy::disallowed_methods,
        reason = "the isolation guard is the one place tests write the process environment"
    )]
    pub fn set(&self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) {
        // SAFETY: set_var is unsafe because another thread may read the
        // environment at the same time. Every test that changes the
        // environment does so through an `IsolatedEnv`, and `&self` proves
        // this call happens while the process-local environment lock is held.
        unsafe { std::env::set_var(key, value) };
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the isolation guard is the one place tests write the process environment"
    )]
    pub fn remove(&self, key: impl AsRef<OsStr>) {
        // SAFETY: as in `set`; `&self` proves the environment lock is held.
        unsafe { std::env::remove_var(key) };
    }

    /// The raw value a variable holds, for a test asserting what the
    /// environment carries. Production code reads through
    /// `shepr_core::env` instead.
    #[expect(
        clippy::disallowed_methods,
        reason = "a test assertion on the raw environment, made under the guard's lock"
    )]
    pub fn get(&self, key: impl AsRef<OsStr>) -> Option<OsString> {
        std::env::var_os(key)
    }
}

impl Default for IsolatedEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for IsolatedEnv {
    fn drop(&mut self) {
        let current: Vec<OsString> = environment_snapshot()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        for key in current {
            if !self.saved.iter().any(|(saved, _)| *saved == key) {
                self.remove(&key);
            }
        }
        for (key, value) in &self.saved {
            if self.get(key).as_ref() != Some(value) {
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
    fn isolated_env_creates_a_private_runtime_directory() {
        use std::os::unix::fs::PermissionsExt;

        let env = IsolatedEnv::new();
        let runtime_dir = env.path().join("runtime");
        let metadata = std::fs::metadata(&runtime_dir).expect("runtime directory exists");

        assert!(metadata.is_dir());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        assert_eq!(
            env.get(EnvVar::XdgRuntimeDir),
            Some(runtime_dir.into_os_string())
        );
    }

    #[test]
    fn isolated_env_clears_every_registered_variable_but_the_scratch_dirs() {
        let env = IsolatedEnv::new();
        // What a developer's shell could hand the test process.
        for var in EnvVar::ALL {
            env.set(var, "/leaked");
        }
        for key in FOREIGN_XDG_BASE_DIR_VARS {
            env.set(key, "/leaked");
        }
        env.set("SHEPR_TEST_SUPPORT_UNREGISTERED", "leaked");

        env.isolate();

        for key in FOREIGN_XDG_BASE_DIR_VARS {
            assert_eq!(env.get(key), None, "{key} leaked into an isolated test");
        }
        for var in EnvVar::ALL {
            match var {
                EnvVar::Home => assert_eq!(env.get(var), Some(env.home().into_os_string())),
                EnvVar::XdgRuntimeDir => {
                    assert_eq!(
                        env.get(var),
                        Some(env.path().join("runtime").into_os_string())
                    );
                }
                _ => assert_eq!(env.get(var), None, "{var} leaked into an isolated test"),
            }
        }
        assert_eq!(env.get("SHEPR_TEST_SUPPORT_UNREGISTERED"), None);
    }

    #[test]
    fn kept_scratch_outside_the_root_is_registered_for_exit_cleanup() {
        let scratch = ScratchDir::new_in(&std::env::temp_dir(), "kept-external");
        let path = scratch.keep_until_exit();

        assert!(path.exists());
        // Only check the registration: running the process-wide cleanup here
        // would delete directories other tests in this process still use.
        let registered = KEPT_SCRATCH_DIRS
            .get()
            .expect("keeping a directory registers it")
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|(owner, kept)| *owner == std::process::id() && *kept == path);
        assert!(registered);
        std::fs::remove_dir_all(&path).expect("remove the kept test directory");
    }
}
