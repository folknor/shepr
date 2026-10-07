//! Every direct filesystem read announces its physical path
//! before entering the kernel. Replacement workers refuse quarantined mounts,
//! or all access when a stalled filesystem was unidentified, including shared
//! config/include and common-directory dependencies.

use crate::worker::RefreshProgress;
use shepr_platform::mounts::{MountTable, PathWalkFailure};
use std::cell::RefCell;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone)]
struct Context {
    progress: RefreshProgress,
    mounts: MountTable,
    git_program: Option<std::ffi::OsString>,
}

thread_local! {
    static CONTEXT: RefCell<Option<Context>> = const { RefCell::new(None) };
    static DENIED: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Whether the last mount table read failed, so an outage is logged when it
/// starts and when it ends rather than on every refresh.
static MOUNT_TABLE_UNREADABLE: AtomicBool = AtomicBool::new(false);

/// Reads this process's mount namespace, logging only a change between
/// readable and unreadable.
pub(crate) fn read_mount_table() -> std::io::Result<MountTable> {
    match MountTable::read() {
        Ok(mounts) => {
            if MOUNT_TABLE_UNREADABLE.swap(false, Ordering::Relaxed) {
                shepr_platform::structured_log!(
                    INFO,
                    event = git.mount_table,
                    outcome = Recovered,
                    "mount table is readable again; Git discovery stops at filesystem boundaries"
                );
            }
            Ok(mounts)
        }
        Err(error) => {
            if !MOUNT_TABLE_UNREADABLE.swap(true, Ordering::Relaxed) {
                shepr_platform::structured_log!(
                    WARN,
                    event = git.mount_table,
                    outcome = Fallback,
                    %error,
                    "failed to read mount table; until it is readable, Git discovery does not \
                     stop at filesystem boundaries, and a Git access that stalls meanwhile \
                     quarantines every path until its thread finishes"
                );
            }
            Err(error)
        }
    }
}

/// The mount snapshot a refresh or an unscoped discovery works from. When
/// mountinfo cannot be read this is the empty table, a degraded mode that keeps
/// Git status computed: no filesystem boundary is known, so discovery may
/// ascend across one, and the quarantine stays sound by being coarse. A step on
/// a path no mount covers is recorded as on an unknown mount (its `/` stall
/// path is diagnostic only), and a stall in it quarantines every path until
/// its thread finishes, through a later readable snapshot too, where `/` would
/// name only the root filesystem. Stuck paths recorded from a readable
/// snapshot list every alias mount point of the stalled device, and an empty
/// table matches them by path prefix.
fn mount_snapshot() -> MountTable {
    read_mount_table().unwrap_or_default()
}

pub(crate) fn scoped<R>(progress: &RefreshProgress, work: impl FnOnce() -> R) -> R {
    scoped_with_mounts(progress, mount_snapshot(), work)
}

pub(crate) fn scoped_with_mounts<R>(
    progress: &RefreshProgress,
    mounts: MountTable,
    work: impl FnOnce() -> R,
) -> R {
    struct Restore(Option<Context>, Option<PathBuf>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CONTEXT.with(|slot| *slot.borrow_mut() = self.0.take());
            DENIED.with(|slot| *slot.borrow_mut() = self.1.take());
        }
    }
    let previous_denied = DENIED.with(std::cell::RefCell::take);
    let previous = CONTEXT.with(|slot| {
        slot.replace(Some(Context {
            progress: progress.clone(),
            mounts,
            git_program: None,
        }))
    });
    let _restore = Restore(previous, previous_denied);
    work()
}

/// The refresh's mount snapshot, or a fresh one outside a refresh; see
/// [`mount_snapshot`] for what an unreadable mountinfo leaves.
pub(crate) fn mount_table() -> MountTable {
    CONTEXT
        .with(|slot| slot.borrow().as_ref().map(|context| context.mounts.clone()))
        .unwrap_or_else(mount_snapshot)
}

/// Refuses `path` if its filesystem is quarantined, and otherwise makes it the
/// refresh's current step until the next announcement.
///
/// The step is deliberately not settled when the access returns. The syscall
/// that follows a walk (`metadata`, `read_link`, the execute check) and the reads on a
/// file `open` returned all run after this, on the announced mount, with no
/// announcement of their own; settled, a read stalled on a hung mount would
/// be abandoned naming no paths, and every replacement would enter that mount
/// again and take another abandonment slot. The price is accepted: a thread
/// that stalls after an access, outside the filesystem, is attributed to the
/// path it last announced, which stays quarantined until that thread finishes.
/// Nothing reaches that today: the work between accesses is in memory, and a
/// Git probe announces its executable and is held to its own deadline, far
/// below the stall bound.
pub(crate) fn announce(path: &Path) -> std::io::Result<()> {
    CONTEXT.with(|slot| {
        if let Some(context) = slot.borrow().as_ref() {
            let (stall_paths, mount_known) = context.mounts.stall_paths_with_coverage(path);
            if !context
                .progress
                .announce(&context.mounts, path, stall_paths, mount_known)
            {
                DENIED.with(|denied| *denied.borrow_mut() = Some(path.to_path_buf()));
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "Git dependency is on a quarantined filesystem",
                ));
            }
        }
        Ok(())
    })
}

// Resolve one component at a time: a single canonicalize/metadata call on a
// logical path could follow a symlink into a hung mount without naming it.
fn resolve(path: &Path, follow_final: bool) -> std::io::Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut pending = std::collections::VecDeque::from_iter(
        path.components()
            .map(|part| part.as_os_str().to_os_string()),
    );
    let mut resolved = PathBuf::new();
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        match Path::new(&part).components().next() {
            Some(Component::RootDir) => {
                resolved = PathBuf::from("/");
                continue;
            }
            Some(Component::CurDir) => continue,
            Some(Component::ParentDir) => {
                resolved.pop();
                continue;
            }
            _ => {}
        }
        resolved.push(part);
        announce(&resolved)?;
        if !follow_final && pending.is_empty() {
            break;
        }
        let metadata = std::fs::symlink_metadata(&resolved)?;
        if metadata.file_type().is_symlink() {
            links += 1;
            if links > 40 {
                return Err(PathWalkFailure::TooManySymlinks.into());
            }
            let target = std::fs::read_link(&resolved)?;
            resolved.pop();
            for component in target.components().rev() {
                pending.push_front(component.as_os_str().to_os_string());
            }
        } else if !pending.is_empty() && !metadata.is_dir() {
            // A non-directory before /.. still cannot be walked.
            return Err(PathWalkFailure::NotDirectory.into());
        }
    }
    announce(&resolved)?;
    Ok(resolved)
}

pub(crate) fn canonicalize(path: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    resolve(path.as_ref(), true)
}

pub(crate) fn has_execute_access(path: &Path) -> std::io::Result<bool> {
    let physical = match resolve(path, true) {
        Ok(physical) => physical,
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Err(error),
        Err(_) => return Ok(false),
    };
    Ok(shepr_platform::has_execute_access(&physical))
}

pub(crate) fn read_link(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::read_link(resolve(path, false)?)
}

pub(crate) fn metadata(path: impl AsRef<Path>) -> std::io::Result<std::fs::Metadata> {
    std::fs::metadata(resolve(path.as_ref(), true)?)
}

pub(crate) fn symlink_metadata(path: impl AsRef<Path>) -> std::io::Result<std::fs::Metadata> {
    std::fs::symlink_metadata(resolve(path.as_ref(), false)?)
}

pub(crate) fn open(path: impl AsRef<Path>) -> std::io::Result<std::fs::File> {
    std::fs::File::open(resolve(path.as_ref(), true)?)
}

pub(crate) fn read_to_string(path: impl AsRef<Path>) -> std::io::Result<String> {
    use std::io::Read;
    let mut result = String::new();
    open(path)?.read_to_string(&mut result)?;
    Ok(result)
}

pub(crate) fn start_job() {
    DENIED.with(|denied| *denied.borrow_mut() = None);
}

pub(crate) fn check_command() -> std::io::Result<()> {
    let cancelled = CONTEXT.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|context| context.progress.is_cancelled())
    });
    DENIED.with(|denied| {
        if cancelled || denied.borrow().is_some() {
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "Git dependency is on a quarantined filesystem",
            ))
        } else {
            Ok(())
        }
    })
}

/// Pin synchronous exec to the mount of the executable, too. PATH search
/// and symlink resolution can block before Git's subprocess deadline exists.
pub(crate) fn git_program() -> std::io::Result<std::ffi::OsString> {
    if CONTEXT.with(|slot| slot.borrow().is_none()) {
        return Ok(std::ffi::OsString::from("git"));
    }
    // Cache only successful resolution within this refresh, not across PATH
    // changes or refreshes. Still announce before every exec so a newly
    // quarantined executable mount is refused and a stalled spawn names it.
    let cached = CONTEXT.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|context| context.git_program.clone())
    });
    if let Some(program) = cached {
        announce(Path::new(&program))?;
        return Ok(program);
    }
    let search = shepr_core::env::read_os(shepr_core::env::EnvVar::Path)
        .map_err(std::io::Error::other)?
        .unwrap_or_else(|| std::ffi::OsString::from("/bin:/usr/bin"));
    for directory in std::env::split_paths(&search) {
        // The runner always execs from /, including relative PATH entries.
        let candidate = Path::new("/").join(directory).join("git");
        match resolve(&candidate, true) {
            Ok(physical) => {
                announce(&physical)?;
                if std::fs::metadata(&physical)?.is_file()
                    && shepr_platform::has_execute_access(&physical)
                {
                    let program = physical.into_os_string();
                    CONTEXT.with(|slot| {
                        if let Some(context) = slot.borrow_mut().as_mut() {
                            context.git_program = Some(program.clone());
                        }
                    });
                    return Ok(program);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::NotADirectory
                        | std::io::ErrorKind::PermissionDenied
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "git executable not found in PATH",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreadable_mount_table_keeps_access_and_quarantines_coarsely() {
        // The empty table is what a refresh runs with when mountinfo cannot
        // be read: access stays available, a path under a stuck root is still
        // refused, and a step is recorded as on an unknown mount, so that a
        // stall in it quarantines every path.
        let progress = RefreshProgress::default();
        progress.set_excluded(vec![PathBuf::from("/net")], false);
        scoped_with_mounts(&progress, MountTable::default(), || {
            start_job();
            assert_eq!(
                metadata("/net/a")
                    .expect_err("a stuck root is refused without mount data")
                    .kind(),
                std::io::ErrorKind::WouldBlock
            );
            start_job();
            announce(Path::new("/srv/b")).expect("other paths stay available");
        });
        assert_eq!(
            progress
                .stalled_paths(std::time::Instant::now() + crate::limits::GIT_REFRESH_STALL_BOUND),
            Some(crate::worker::StalledPaths {
                paths: vec![PathBuf::from("/")],
                unknown_mount: true,
            })
        );
    }

    #[test]
    fn executable_resolution_is_cached_only_inside_the_refresh() {
        let env = shepr_test_support::IsolatedEnv::new();
        let scratch = shepr_test_support::ScratchDir::new("git-program-cache");
        let program = shepr_test_support::fixture::stand_in(scratch.path(), "git", &[]);
        env.set(shepr_core::env::EnvVar::Path, scratch.path());
        let progress = RefreshProgress::default();
        // A synthetic single-device namespace makes exclusion independent of
        // whether the scratch directory sits on a separate host mount.
        let mounts = MountTable::from_mountinfo("1 0 8:1 / / rw - ext4 root rw\n");
        scoped_with_mounts(&progress, mounts.clone(), || {
            let first = git_program().expect("resolve executable");
            std::fs::remove_file(&program).expect("remove fixture");
            assert_eq!(git_program().expect("cached executable"), first);
            progress.set_excluded(vec![PathBuf::from("/")], false);
            assert_eq!(
                git_program().err().map(|error| error.kind()),
                Some(std::io::ErrorKind::WouldBlock)
            );
        });
        progress.set_excluded(Vec::new(), false);
        scoped_with_mounts(&progress, mounts, || {
            assert_eq!(
                git_program().err().map(|error| error.kind()),
                Some(std::io::ErrorKind::NotFound)
            );
        });
    }

    #[test]
    fn sibling_checkouts_and_shared_dependencies_refuse_a_stalled_mount() {
        let progress = RefreshProgress::default();
        progress.set_excluded(vec![PathBuf::from("/net")], false);
        let mounts = MountTable::from_mountinfo(
            "1 0 8:1 / / rw - ext4 root rw\n2 1 0:42 / /net rw - nfs host:/ rw\n",
        );
        scoped_with_mounts(&progress, mounts, || {
            for path in [
                "/net/a",
                "/net/b",
                "/net/home/.gitconfig",
                "/net/includes/git",
                "/net/common/config",
            ] {
                start_job();
                assert_eq!(
                    metadata(path)
                        .expect_err("no access to quarantined mount")
                        .kind(),
                    std::io::ErrorKind::WouldBlock
                );
                assert!(
                    check_command().is_err(),
                    "do not invoke Git after refusing a dependency"
                );
            }
            start_job();
            assert!(
                metadata("/").is_ok(),
                "healthy filesystem remains available"
            );
            assert!(check_command().is_ok());
        });
    }

    #[test]
    fn symlink_dependency_names_its_target_mount_before_access() {
        let scratch = shepr_test_support::ScratchDir::new("git-access-symlink");
        let link = scratch.join("config");
        let target = PathBuf::from("/net/config");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let progress = RefreshProgress::default();
        progress.set_excluded(vec![PathBuf::from("/net")], false);
        let mounts = MountTable::from_mountinfo(
            "1 0 8:1 / / rw - ext4 root rw\n2 1 0:42 / /net rw - nfs host:/ rw\n",
        );
        scoped_with_mounts(&progress, mounts, || {
            assert_eq!(
                open(&link).expect_err("target mount refused").kind(),
                std::io::ErrorKind::WouldBlock
            );
        });
    }

    #[test]
    fn progress_names_mount_aliases_instead_of_checkout() {
        let progress = RefreshProgress::default();
        let mounts = MountTable::from_mountinfo(
            "1 0 8:1 / / rw - ext4 root rw\n2 1 0:42 / /net rw - nfs host:/ rw\n3 1 0:42 / /alias rw - nfs host:/ rw\n",
        );
        scoped_with_mounts(&progress, mounts, || announce(Path::new("/net/a"))).expect("announce");
        assert_eq!(
            progress
                .stalled_paths(std::time::Instant::now() + crate::limits::GIT_REFRESH_STALL_BOUND),
            Some(crate::worker::StalledPaths {
                paths: vec![PathBuf::from("/net"), PathBuf::from("/alias")],
                unknown_mount: false,
            })
        );
    }
}
