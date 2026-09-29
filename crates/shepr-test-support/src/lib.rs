//! Test isolation shared by every unit test in the workspace.
//!
//! Three rules, each implemented once, here:
//!
//! - A test that needs a process spawns the workspace-built fixture program
//!   ([`fixture`]), never a program borrowed from the host (`sh`, `sleep`,
//!   `printf`, `cat`, an authored shell script, ...). The exceptions
//!   are tests whose subject is a script itself and tests of production code
//!   that spawns a host program; each is marked at its site, and
//!   `brokkr.toml`'s host-program textlints hold the rest.
//! - Scratch files live in a [`ScratchDir`]: a private directory under the
//!   project's build tree, cleared when it is handed out. Never the host's
//!   temp directory (`clippy.toml` bans `std::env::temp_dir` with no escape),
//!   never a fixed or shared path, and never anything under the real `$HOME`.
//! - A test that changes the process environment, or reads a variable (or the
//!   explicit-session flag) that another test changes, holds an
//!   [`IsolatedEnv`] for its whole body. These tests serialize with one another
//!   when they run in the same test process, whichever module they live in.
//!   The guard clears every name in shepr's process environment registry and
//!   each shepr-specific child environment name, including Git's indexed
//!   command-config family, then points `HOME` and
//!   `XDG_RUNTIME_DIR` at scratch and sets `GIT_CEILING_DIRECTORIES` to
//!   [`scratch_base`], so a scratch
//!   directory is never discovered as part of the enclosing checkout, and
//!   sets `GIT_CONFIG_NOSYSTEM` so the host's system Git config is never
//!   read. It also clears the XDG base directories shepr does not read but the
//!   tools tests spawn do. Nothing under test can reach the user's real
//!   config, state or agent directories, or the live shepr server a test run
//!   was started from. It restores the whole environment when dropped,
//!   including on panic.
//!
//! Prefer passing a value in over setting an environment variable: code that
//! takes the path or setting as an argument needs neither the lock nor the
//! guard.
//!
//! This crate is where tests touch the process environment directly: the
//! guard's snapshot, writes and restore, [`IsolatedEnv::get`] for a test
//! asserting what a variable holds, and the one read of [`SCRATCH_DIR_ENV`].
//! Each such site carries an `#[expect]`.
//!
//! # Where scratch trees live
//!
//! Every tree is a direct child of [`scratch_base`]: `target/t` in the
//! workspace, or [`SCRATCH_DIR_ENV`] when set. Its name is a fixed-width
//! digest of three things: the test executable's identity (its file stem
//! without cargo's build hash), this process's slot, and the caller's label
//! made unique per call. The slot is the lowest of a small pool of lock files
//! this process holds an advisory lock on for its whole life, so two live
//! processes of one test binary (a rerun overlapping a slow run, or a test
//! that re-executes its own binary) get disjoint trees, and a crashed process
//! releases its slot with no liveness guessing.
//!
//! A tree is cleared when it is handed out and left in place afterwards. A
//! rerun takes slot 0 again, derives the same names, and clears the same
//! directories, so the build tree holds one generation of each test's scratch
//! per slot however the previous run ended: passed, failed, or killed.
//!
//! Two designs were rejected, so they are not proposed again.
//!
//! - Naming trees by pid and sweeping the ones whose pid is dead. A pid is new
//!   every run, so nothing is ever reused and everything depends on the sweep,
//!   which is unsound however it is arranged: between reading a dead pid and
//!   removing its tree, the pid can be recycled and the tree rebound by a live
//!   process, and the sweep deletes a fixture in use. Cleaning up at exit
//!   instead (`atexit`) never runs for a process killed with SIGKILL, so every
//!   killed run left its tree behind for good.
//! - A guard whose `Drop` removes the tree. `ScratchDir::new("x").join("y")` is
//!   ordinary Rust that deletes the directory before the caller opens the path,
//!   so such a guard needs an opt-out for fixtures that outlive it, and every
//!   opted-out tree, like every test that cleaned up by hand after its
//!   assertions, leaked whenever the test failed first.
//!
//! # The socket budget
//!
//! Tests bind Unix sockets in their scratch trees, and a socket path must fit
//! the kernel's socket address (`shepr_core::socket_path`). Every scratch root
//! has the same length, the base plus one separator plus [`DIGEST_WIDTH`]
//! characters, so the room left below it is a property of the checkout, not
//! of the test. Each [`ScratchDir`] proves that a name of
//! [`SOCKET_LEAF_BUDGET`] bytes still fits below its root and refuses,
//! naming [`SCRATCH_DIR_ENV`] as the remedy, when a checkout is too deep for
//! it. That is one refusal for the whole suite rather than scattered socket
//! tests failing on a precondition none of them is about.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions, TryLockError};
use std::hash::{Hash, Hasher};
use std::io;
use std::ops::Deref;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use shepr_core::env::{ChildEnv, EnvVar, is_registered_name};
use shepr_core::socket_path::fits_unix_socket_path;

pub mod fixture;

/// The environment variable naming where scratch trees are sited instead of
/// the workspace's `target/t`.
///
/// It exists because the default base sits under the checkout, whose depth
/// the suite does not choose: a checkout too deep for [`SOCKET_LEAF_BUDGET`],
/// or a `target` relocated onto a filesystem that cannot hold what a test
/// needs, is answered by pointing this at a shorter or better path rather than
/// by moving the checkout. It must be absolute. No shepr process reads it, so
/// it is not in [`EnvVar`]'s process-input table; [`IsolatedEnv`] leaves it in
/// place so a test binary a test re-executes resolves the same base.
pub const SCRATCH_DIR_ENV: &str = "SHEPR_TEST_SCRATCH_DIR";

/// The longest socket name, in bytes, every scratch root leaves room for
/// directly below it.
///
/// Sized to the deepest socket tests bind straight into a scratch root: the
/// remote bridge's hashed fallback name,
/// `shepr-r-<pid>-<target prefix>-<hash>.<token>.sock`, with a seven-digit pid
/// (the largest Linux hands out), an eight-character target prefix and two
/// sixteen-digit hex fields.
///
/// Deliberately not budgeted: the shared OpenSSH control socket, whose
/// staging name leaves room only for a runtime directory as short as a real
/// `/run/user/<uid>`, which no directory under a checkout's build tree is.
/// Tests of that name exercise its arithmetic over the real directory's
/// spelling instead.
pub const SOCKET_LEAF_BUDGET: usize = 63;

/// The width of every scratch root's name.
///
/// Eight characters from a 62-letter alphabet carry about 47 bits, near the
/// 48 of twelve hex digits, in four fewer bytes, and every byte of a root's
/// name comes out of [`SOCKET_LEAF_BUDGET`]'s room. The bits are an ownership
/// argument: two live roots colliding means two processes clear and reuse one
/// tree while each believes it owns it, which the per-process claim registry
/// cannot see, and which would present as socket or lifecycle nondeterminism in
/// an unrelated subsystem.
pub const DIGEST_WIDTH: usize = 8;

const DIGEST_ALPHABET: &[u8; 62] =
    b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
const DIGEST_RADIX: u64 = 62;

/// How many live processes of one test executable may hold scratch trees at
/// once. Exhaustion refuses rather than sharing a slot, since sharing one
/// means clearing a tree another process is using.
const SLOT_CAPACITY: u32 = 16;

/// The directory under the base holding each executable's slot lock files.
/// Dot-named so it can never be a digest.
const SLOT_LOCK_DIR: &str = ".slots";

const PRIVATE_DIR_MODE: u32 = 0o700;

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

/// The raw [`SCRATCH_DIR_ENV`] value, empty read as unset.
///
/// Read once per process, when [`scratch_base`] is first resolved. That is
/// before any [`IsolatedEnv`] changes the environment: the guard creates its
/// own scratch directory, and so resolves the base, before it clears anything.
#[expect(
    clippy::disallowed_methods,
    reason = "a test harness variable no shepr process interprets, outside the environment reader's registry"
)]
fn scratch_dir_override() -> Option<OsString> {
    std::env::var_os(SCRATCH_DIR_ENV).filter(|value| !value.is_empty())
}

/// Where the scratch base was asked to be, before it is created and resolved.
fn scratch_base_request() -> PathBuf {
    if let Some(value) = scratch_dir_override() {
        let path = PathBuf::from(value);
        assert!(
            path.is_absolute(),
            "{SCRATCH_DIR_ENV} is {}, which is a relative path; set it to an absolute path or \
             unset it",
            path.display()
        );
        return path;
    }
    // This crate is `crates/shepr-test-support`, so `../..` is the workspace
    // root. Resolved before `target/t` is appended, so a `target` symlinked
    // elsewhere is followed rather than named.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|error| panic!("resolving the workspace root: {error}"))
        .join("target/t")
}

/// The one canonical directory every scratch tree is created under, resolved
/// once per process.
///
/// Canonical, because production resolves what it is given: code under test
/// that canonicalizes a path and compares it with the one it was handed would
/// otherwise see two spellings of one directory.
///
/// # Panics
///
/// When the base cannot be created or resolved, naming [`SCRATCH_DIR_ENV`] as
/// the remedy.
#[must_use]
pub fn scratch_base() -> &'static Path {
    static BASE: OnceLock<PathBuf> = OnceLock::new();
    BASE.get_or_init(|| {
        let requested = scratch_base_request();
        std::fs::create_dir_all(&requested).unwrap_or_else(|error| {
            panic!(
                "creating the scratch base {}: {error}. This is a suite-wide precondition, not a \
                 failure of whichever test resolved it first: set {SCRATCH_DIR_ENV} to a writable \
                 absolute path.",
                requested.display()
            )
        });
        requested.canonicalize().unwrap_or_else(|error| {
            panic!(
                "resolving the scratch base {}: {error}. This is a suite-wide precondition, not a \
                 failure of whichever test resolved it first: set {SCRATCH_DIR_ENV} to a writable \
                 absolute path.",
                requested.display()
            )
        })
    })
}

/// This process's slot among the live processes of its test executable.
struct Slot {
    /// The executable's identity, shared by every build of one test target.
    namespace: String,
    index: u32,
    /// Holding the advisory lock is the ownership; it is released when the
    /// process exits, however it exits.
    _lock: File,
}

fn slot() -> &'static Slot {
    static SLOT: OnceLock<Slot> = OnceLock::new();
    SLOT.get_or_init(acquire_slot)
}

fn acquire_slot() -> Slot {
    let exe = std::env::current_exe().unwrap_or_else(|error| {
        // Refusing is mandatory: a shared fallback name would let two
        // executables clear each other's trees.
        panic!("scratch trees need this test executable's identity: {error}")
    });
    let namespace = stable_executable_identity(&exe);
    let locks = scratch_base().join(SLOT_LOCK_DIR).join(&namespace);
    std::fs::create_dir_all(&locks).unwrap_or_else(|error| {
        panic!(
            "creating the scratch slot directory {}: {error}",
            locks.display()
        )
    });
    let Some((index, lock)) = try_claim_slot(&locks, SLOT_CAPACITY) else {
        panic!(
            "all {SLOT_CAPACITY} scratch slots of {namespace:?} are held by other live processes; \
             wait for them rather than sharing a slot, which would clear a tree another process is \
             using"
        );
    };
    Slot {
        namespace,
        index,
        _lock: lock,
    }
}

/// Claims the lowest free slot below `capacity`, or `None` when every one is
/// held. The capacity is a parameter so a test can prove exhaustion with a
/// few held locks instead of a full pool of processes.
fn try_claim_slot(locks: &Path, capacity: u32) -> Option<(u32, File)> {
    for index in 0..capacity {
        let path = locks.join(index.to_string());
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .unwrap_or_else(|error| panic!("opening scratch slot {}: {error}", path.display()));
        match file.try_lock() {
            Ok(()) => return Some((index, file)),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => {
                panic!("locking scratch slot {}: {error}", path.display())
            }
        }
    }
    None
}

/// A test executable's identity across rebuilds: its file stem with cargo's
/// trailing `-<build hash>` removed.
///
/// Cargo's hash moves with features, profile, compiler version and the
/// dependency graph, so keying on it would open a fresh set of trees for every
/// build configuration and abandon the previous set intact. The directory is
/// dropped too, so a relocated target directory's copy of one test binary is
/// the same test binary. Stripping is conservative: only a trailing `-` plus
/// lowercase hex is removed. Two targets that still collide on a stem take
/// separate slots, because the slot is a lock, not a naming convention.
fn stable_executable_identity(exe: &Path) -> String {
    let stem = exe
        .file_stem()
        .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
    match stem.rsplit_once('-') {
        Some((head, tail))
            if !head.is_empty()
                && !tail.is_empty()
                && tail.chars().all(|character| {
                    character.is_ascii_digit() || matches!(character, 'a'..='f')
                }) =>
        {
            head.to_owned()
        }
        _ => stem,
    }
}

/// A name unique to this call: the label, the calling thread's name and a
/// sequence per label and thread.
///
/// libtest runs each test on a thread named after the test, also at
/// `--test-threads=1`, so a rerun derives the same names and lands on the same
/// trees. Nothing relies on that for correctness: the claim registry is what
/// keeps two live fixtures apart, under any scheduling.
fn per_call_name(label: &str) -> String {
    static SEQUENCES: OnceLock<Mutex<HashMap<(String, String), u64>>> = OnceLock::new();
    let thread = std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .to_owned();
    let sequence = {
        let mut sequences = SEQUENCES
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let next = sequences
            .entry((label.to_owned(), thread.clone()))
            .or_insert(0);
        let current = *next;
        *next += 1;
        current
    };
    format!("{label}~{thread}~{sequence}")
}

/// Records exclusive ownership of one scratch directory for this process.
///
/// Keyed by the resolved directory, so a digest collision between two names
/// is caught as well as a repeated name.
///
/// # Panics
///
/// Naming both claimants. A tree is cleared when it is claimed, so a second
/// claim would delete the first fixture's live tree.
fn claim_scratch_path(path: &Path, name: &str) {
    static CLAIMED: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();
    let mut claimed = CLAIMED
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(previous) = claimed.get(path) {
        if previous == name {
            panic!(
                "scratch name {name:?} was claimed twice in this process; a tree is cleared when \
                 it is claimed, so the second claim would delete the first fixture's tree"
            );
        }
        panic!(
            "scratch names {previous:?} and {name:?} both resolve to the directory {}",
            path.display()
        );
    }
    claimed.insert(path.to_path_buf(), name.to_owned());
}

/// The fixed-width name of one claimed scratch name's root.
fn root_name(name: &str) -> String {
    let slot = slot();
    digest(&[
        slot.namespace.as_bytes(),
        &slot.index.to_le_bytes(),
        name.as_bytes(),
    ])
}

/// [`DIGEST_WIDTH`] alphanumeric characters, zero-padded, so every root name
/// has the same length.
///
/// `DefaultHasher` is enough: its algorithm may change between Rust releases,
/// and the only consequence is that one generation of scratch trees stops
/// being reused and is left behind as disposable build output. Within one run
/// every executable was built against one standard library and agrees on the
/// mapping.
fn digest(parts: &[&[u8]]) -> String {
    let mut hasher = DefaultHasher::new();
    for part in parts {
        part.hash(&mut hasher);
    }
    let mut value = hasher.finish();
    (0..DIGEST_WIDTH)
        .map(|_| {
            let index = usize::try_from(value % DIGEST_RADIX).expect("a digit below the radix");
            value /= DIGEST_RADIX;
            char::from(DIGEST_ALPHABET[index])
        })
        .collect()
}

/// Refuses a root that leaves less than [`SOCKET_LEAF_BUDGET`] bytes for a
/// socket name below it.
fn prove_socket_budget(root: &Path) {
    let deepest = root.join("x".repeat(SOCKET_LEAF_BUDGET));
    assert!(
        fits_unix_socket_path(&deepest),
        "the scratch root {} leaves less than {SOCKET_LEAF_BUDGET} bytes for a socket name below \
         it, so socket tests could not bind. This is a suite-wide precondition, not a failure of \
         this test: set {SCRATCH_DIR_ENV} to a shorter absolute path.",
        root.display()
    );
}

/// Clears `path` and recreates it empty at mode `0700`, refusing to adopt an
/// existing symlink or file as the directory.
///
/// # Panics
///
/// On any filesystem failure, or when the created directory reads back with a
/// group or other bit set.
fn recreate_private_dir(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => clear_tree(path),
        Ok(_) => std::fs::remove_file(path).unwrap_or_else(|error| {
            panic!("removing scratch occupier {}: {error}", path.display())
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => panic!("inspecting scratch {}: {error}", path.display()),
    }
    std::fs::DirBuilder::new()
        .mode(PRIVATE_DIR_MODE)
        .create(path)
        .unwrap_or_else(|error| panic!("creating scratch {}: {error}", path.display()));
    let mode = std::fs::metadata(path)
        .unwrap_or_else(|error| panic!("reading scratch {}: {error}", path.display()))
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o077,
        0,
        "scratch {} is not private: mode {:04o}",
        path.display(),
        mode & 0o7777
    );
}

/// Removes a previous run's tree. A test that took permissions away from a
/// directory in its tree (to exercise an unreadable path) and never gave them
/// back, because it failed or was killed first, would otherwise make every
/// later run of it fail here; the owner may always restore them.
fn clear_tree(path: &Path) {
    if std::fs::remove_dir_all(path).is_ok() {
        return;
    }
    restore_owner_access(path);
    std::fs::remove_dir_all(path)
        .unwrap_or_else(|error| panic!("clearing scratch {}: {error}", path.display()));
}

/// Gives the owner full access to every directory in the tree, without
/// following symlinks. Errors are left for the retried removal to report.
fn restore_owner_access(dir: &Path) {
    // A failure here surfaces as the retried removal's panic, which names the
    // path; reporting it twice adds nothing.
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(PRIVATE_DIR_MODE)).ok();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            restore_owner_access(&entry.path());
        }
    }
}

/// A private (0700) directory for one test, cleared when it is handed out and
/// deliberately left in place afterwards (see the crate docs for why).
///
/// Each call gets its own directory, also for a repeated label, so a helper
/// may call it with a fixed label. The label only has to make the tree
/// recognisable to the code that made it; its length never reaches the path.
#[derive(Debug)]
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    /// # Panics
    ///
    /// When the scratch base cannot be resolved, every slot is held, the root
    /// leaves no room for [`SOCKET_LEAF_BUDGET`], or the directory cannot be
    /// created private.
    #[must_use]
    pub fn new(label: &str) -> Self {
        let name = per_call_name(label);
        let path = scratch_base().join(root_name(&name));
        claim_scratch_path(&path, &name);
        prove_socket_budget(&path);
        recreate_private_dir(&path);
        Self { path }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
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

/// A child process command whose working directory is a fresh [`ScratchDir`]
/// labelled `label`, so a test's child never runs in, writes into, or resolves
/// relative paths against the directory the test run started in. A test that
/// needs the child somewhere specific overrides it with `current_dir`.
///
/// # Panics
///
/// As [`ScratchDir::new`].
#[must_use]
pub fn command_in_scratch(program: impl AsRef<OsStr>, label: &str) -> std::process::Command {
    let scratch = ScratchDir::new(label);
    #[expect(
        clippy::disallowed_methods,
        reason = "the tests' shared constructor; it states the scratch working directory on the next line"
    )]
    let mut command = std::process::Command::new(program);
    command.current_dir(scratch.path());
    command
}

/// Exclusive, restorable access to the process environment for one test.
///
/// Holding it serializes the test against other tests in its process that hold
/// one. On creation it snapshots the environment and removes every registered
/// process variable, every shepr-specific child variable except
/// [`SCRATCH_DIR_ENV`], and the foreign XDG base directories. It then sets
/// `HOME` and `XDG_RUNTIME_DIR` to fresh scratch directories and
/// `GIT_CEILING_DIRECTORIES` to the scratch base. On drop it puts the snapshot
/// back exactly.
///
/// Change variables through [`IsolatedEnv::set`] and [`IsolatedEnv::remove`]:
/// borrowing the guard is what proves the lock is held.
pub struct IsolatedEnv {
    saved: Vec<(OsString, OsString)>,
    scratch: ScratchDir,
    // Declared last so it is released after the environment is restored.
    _lock: MutexGuard<'static, ()>,
}

impl IsolatedEnv {
    /// # Panics
    ///
    /// As [`ScratchDir::new`], or when the scratch `HOME` or runtime directory
    /// cannot be created.
    #[must_use]
    pub fn new() -> Self {
        // A test that panicked while holding the lock still restored the
        // environment on unwind, so a poisoned lock carries no bad state.
        let lock = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let saved = environment_snapshot();
        let scratch = ScratchDir::new("env");
        let env = Self {
            saved,
            scratch,
            _lock: lock,
        };
        for dir in [env.home(), env.runtime_dir()] {
            std::fs::DirBuilder::new()
                .mode(PRIVATE_DIR_MODE)
                .create(&dir)
                .unwrap_or_else(|error| panic!("creating {}: {error}", dir.display()));
        }
        env.isolate();
        env
    }

    /// Clears everything a test must not inherit, points `HOME` and
    /// `XDG_RUNTIME_DIR` at this guard's scratch directories, puts a Git
    /// ceiling at the scratch base and turns off Git's system config.
    fn isolate(&self) {
        let registered: Vec<OsString> = environment_snapshot()
            .into_iter()
            .map(|(key, _)| key)
            .filter(|key| is_registered_name(key))
            .collect();
        for key in registered {
            self.remove(key);
        }
        for key in FOREIGN_XDG_BASE_DIR_VARS {
            self.remove(key);
        }
        // Clear the registered SHEPR_* names shepr writes for hooks and
        // status commands. The scratch override stays, so a re-executed test
        // binary sites its trees where this one does.
        for variable in ChildEnv::ALL {
            if variable.name().starts_with("SHEPR_") && variable.name() != SCRATCH_DIR_ENV {
                self.remove(variable);
            }
        }
        self.set(EnvVar::Home, self.home());
        self.set(EnvVar::XdgRuntimeDir, self.runtime_dir());
        // The scratch base sits inside the checkout, so without a ceiling a
        // scratch directory that is not a repository would be discovered as
        // part of the checkout, by shepr's discovery and by Git alike.
        self.set(EnvVar::GitCeilingDirectories, scratch_base());
        // The host's system Git config (`/etc/gitconfig`) is outside the
        // scratch tree; neither shepr's discovery nor a spawned Git may read
        // it. A test of the system level removes this again.
        self.set(EnvVar::GitConfigNoSystem, "1");
    }

    fn runtime_dir(&self) -> PathBuf {
        self.scratch.join("runtime")
    }

    /// This test's scratch directory. `HOME` is its `home` subdirectory.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.scratch.path()
    }

    /// The scratch directory `HOME` points at.
    #[must_use]
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
    #[must_use]
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

    fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| {
                payload
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_owned())
            })
            .unwrap_or_default()
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("test precondition")
            .permissions()
            .mode()
            & 0o7777
    }

    #[test]
    fn scratch_dirs_are_private_distinct_fixed_width_and_under_the_base() {
        let first = ScratchDir::new("a");
        let second = ScratchDir::new("a");
        assert_ne!(first.path(), second.path());
        for scratch in [&first, &second] {
            assert_eq!(scratch.parent(), Some(scratch_base()));
            assert_eq!(mode(scratch), 0o700);
            let name = scratch
                .file_name()
                .and_then(OsStr::to_str)
                .expect("a digest name");
            assert_eq!(name.len(), DIGEST_WIDTH);
            assert!(name.bytes().all(|byte| DIGEST_ALPHABET.contains(&byte)));
        }
    }

    /// The budget is proven by binding, not only by measuring: a socket with a
    /// budget-long name binds directly below a scratch root.
    #[test]
    fn a_budget_long_socket_name_binds_below_a_scratch_root() {
        let root = ScratchDir::new("socket-budget");
        let socket = root.join("x".repeat(SOCKET_LEAF_BUDGET));
        let listener = std::os::unix::net::UnixListener::bind(&socket)
            .expect("a budget-long socket name binds below a scratch root");
        drop(listener);
    }

    /// The budget is proven against roots whose name is a fixed width, so the
    /// width is part of that proof. A small hash is zero-padded rather than
    /// shortened, or two roots could differ only in length.
    #[test]
    fn digests_are_fixed_width_alphanumeric() {
        let identity: &[&[u8]] = &[b"identity"];
        let empty: &[&[u8]] = &[];
        for parts in [identity, empty] {
            let name = digest(parts);
            assert_eq!(name.len(), DIGEST_WIDTH, "{name}");
            assert!(
                name.bytes().all(|byte| byte.is_ascii_alphanumeric()),
                "{name}"
            );
        }
        assert_eq!(
            usize::try_from(DIGEST_RADIX).expect("a small radix"),
            DIGEST_ALPHABET.len()
        );
    }

    #[test]
    fn the_executable_identity_drops_only_cargos_build_hash() {
        let identity = |path: &str| stable_executable_identity(Path::new(path));
        assert_eq!(
            identity("/w/target/debug/deps/shepr_mux-0123456789abcdef"),
            "shepr_mux"
        );
        assert_eq!(
            identity("/elsewhere/shepr_mux-0123456789abcdef"),
            "shepr_mux"
        );
        assert_eq!(identity("/w/target/debug/deps/cli"), "cli");
        assert_eq!(identity("/w/bin/shepr-Release"), "shepr-Release");
        assert_eq!(identity("/w/bin/shepr-"), "shepr-");
    }

    /// One label called from two named threads, twice each, gives four
    /// disjoint trees.
    #[test]
    fn repeated_labels_across_threads_get_disjoint_trees() {
        let roots: Vec<PathBuf> = ["scratch-thread-a", "scratch-thread-b"]
            .into_iter()
            .map(|name| {
                std::thread::Builder::new()
                    .name(name.to_owned())
                    .spawn(|| {
                        [ScratchDir::new("shared"), ScratchDir::new("shared")]
                            .map(|scratch| scratch.to_path_buf())
                    })
                    .expect("spawning a named thread")
            })
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(|handle| handle.join().expect("the thread completed"))
            .collect();
        let unique: std::collections::HashSet<&PathBuf> = roots.iter().collect();
        assert_eq!(unique.len(), roots.len(), "{roots:?}");
    }

    /// A claim clears the tree, so claiming one directory twice would delete
    /// the first fixture's tree under it; both shapes refuse, naming the names.
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "the refusal under test is a panic; catching it is how the test reads its message"
    )]
    fn a_directory_cannot_be_claimed_twice() {
        let path = scratch_base().join("claim-registry-probe");
        claim_scratch_path(&path, "first");
        let again = std::panic::catch_unwind(|| claim_scratch_path(&path, "first"))
            .expect_err("a repeated name refuses");
        assert!(panic_message(&*again).contains("claimed twice"));
        let alias = std::panic::catch_unwind(|| claim_scratch_path(&path, "second"))
            .expect_err("a second name for one directory refuses");
        let message = panic_message(&*alias);
        assert!(
            message.contains("\"first\"") && message.contains("\"second\""),
            "{message}"
        );
    }

    /// The slot scan's capacity is an input: two held locks exhaust a
    /// two-slot scan, and a three-slot scan finds the third slot free.
    #[test]
    fn the_slot_scan_skips_held_slots_and_refuses_when_all_are_held() {
        let locks = scratch_base()
            .join(SLOT_LOCK_DIR)
            .join("shepr-test-support-injected-capacity");
        std::fs::create_dir_all(&locks).expect("test precondition");
        let hold = |index: u32| {
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(locks.join(index.to_string()))
                .expect("test precondition");
            file.try_lock().expect("the slot is free");
            file
        };
        let _zero = hold(0);
        let _one = hold(1);
        assert!(try_claim_slot(&locks, 2).is_none());
        let (index, _two) = try_claim_slot(&locks, 3).expect("the third slot is free");
        assert_eq!(index, 2);
    }

    #[test]
    fn a_reclaimed_tree_is_cleared_even_where_a_test_left_it_unreadable() {
        let scratch = ScratchDir::new("stale");
        let tree = scratch.join("tree");
        let locked = tree.join("locked");
        std::fs::create_dir_all(&locked).expect("test precondition");
        std::fs::write(locked.join("stale"), "old").expect("test precondition");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("test precondition");

        recreate_private_dir(&tree);

        assert_eq!(
            std::fs::read_dir(&tree).expect("test precondition").count(),
            0,
            "the recreated tree is empty"
        );
        assert_eq!(mode(&tree), 0o700);
    }

    /// A leaf at the scratch path is removed rather than adopted, whatever it
    /// is - a symlink pointing elsewhere most of all.
    #[test]
    fn a_planted_symlink_is_removed_rather_than_followed() {
        let root = ScratchDir::new("planted-symlink");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir(&elsewhere).expect("test precondition");
        std::fs::write(elsewhere.join("witness"), "keep me").expect("test precondition");
        let leaf = root.join("leaf");
        std::os::unix::fs::symlink(&elsewhere, &leaf).expect("test precondition");

        recreate_private_dir(&leaf);

        assert!(
            std::fs::symlink_metadata(&leaf)
                .expect("the leaf exists")
                .file_type()
                .is_dir(),
            "the leaf is a real directory"
        );
        assert!(
            std::fs::symlink_metadata(elsewhere.join("witness"))
                .expect("the link target is untouched")
                .is_file(),
            "the link target is untouched"
        );
    }

    #[test]
    fn isolated_env_creates_a_private_runtime_directory() {
        let env = IsolatedEnv::new();
        let runtime_dir = env.path().join("runtime");
        let metadata = std::fs::metadata(&runtime_dir).expect("runtime directory exists");

        assert!(metadata.is_dir());
        assert_eq!(mode(&runtime_dir), 0o700);
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
        for variable in ChildEnv::ALL {
            if variable.name().starts_with("SHEPR_") && variable.name() != SCRATCH_DIR_ENV {
                env.set(variable, "leaked");
            }
        }
        env.set("GIT_CONFIG_COUNT", "2");
        env.set("GIT_CONFIG_KEY_0", "core.bare");
        env.set("GIT_CONFIG_VALUE_0", "true");
        env.set("GIT_CONFIG_KEY_1", "core.filemode");
        env.set("GIT_CONFIG_VALUE_1", "false");
        env.set(SCRATCH_DIR_ENV, "/kept");

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
                EnvVar::GitCeilingDirectories => {
                    assert_eq!(env.get(var), Some(scratch_base().as_os_str().to_owned()));
                }
                EnvVar::GitConfigNoSystem => assert_eq!(env.get(var), Some(OsString::from("1"))),
                _ => assert_eq!(env.get(var), None, "{var} leaked into an isolated test"),
            }
        }
        for variable in ChildEnv::ALL {
            if variable.name().starts_with("SHEPR_") && variable.name() != SCRATCH_DIR_ENV {
                assert_eq!(
                    env.get(variable),
                    None,
                    "{} leaked into an isolated test",
                    variable.name()
                );
            }
        }
        for key in [
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "GIT_CONFIG_KEY_1",
            "GIT_CONFIG_VALUE_1",
        ] {
            assert_eq!(env.get(key), None, "{key} leaked into an isolated test");
        }
        assert_eq!(env.get(SCRATCH_DIR_ENV), Some(OsString::from("/kept")));
    }
}
