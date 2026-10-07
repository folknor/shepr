//! Every direct filesystem read announces its physical path
//! before entering the kernel. Replacement workers refuse quarantined mounts,
//! including shared config/include and common-directory dependencies.

use crate::worker::RefreshProgress;
use shepr_platform::mounts::{MountTable, PathWalkFailure};
use std::cell::RefCell;
use std::path::{Component, Path, PathBuf};

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

pub(crate) fn scoped<R>(progress: &RefreshProgress, work: impl FnOnce() -> R) -> R {
    scoped_with_mounts(progress, MountTable::read().unwrap_or_default(), work)
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

pub(crate) fn mount_table() -> std::io::Result<MountTable> {
    if let Some(mounts) =
        CONTEXT.with(|slot| slot.borrow().as_ref().map(|context| context.mounts.clone()))
    {
        Ok(mounts)
    } else {
        MountTable::read()
    }
}

fn announce(path: &Path) -> std::io::Result<()> {
    CONTEXT.with(|slot| {
        if let Some(context) = slot.borrow().as_ref() {
            let stall_paths = context.mounts.stall_paths(path);
            if context
                .progress
                .excludes(&context.mounts, path, &stall_paths)
            {
                DENIED.with(|denied| *denied.borrow_mut() = Some(path.to_path_buf()));
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "Git dependency is on a quarantined filesystem",
                ));
            }
            context.progress.step(stall_paths);
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
            progress.set_excluded(vec![PathBuf::from("/")]);
            assert_eq!(
                git_program().err().map(|error| error.kind()),
                Some(std::io::ErrorKind::WouldBlock)
            );
        });
        progress.set_excluded(Vec::new());
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
        progress.set_excluded(vec![PathBuf::from("/net")]);
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
        progress.set_excluded(vec![PathBuf::from("/net")]);
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
            Some(vec![PathBuf::from("/net"), PathBuf::from("/alias")])
        );
    }
}
