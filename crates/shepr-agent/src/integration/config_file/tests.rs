use super::*;
use crate::integration::test_support::symlink_file;
use shepr_test_support::IsolatedEnv;

/// A scratch tree, left in place afterwards as every `ScratchDir` is: the next
/// run clears it when it is handed out.
struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        Self(shepr_test_support::ScratchDir::new("config-write").to_path_buf())
    }
}

const ATOMIC_CASES: &[bool] = &[false, true];

#[test]
fn config_publication_keeps_old_content_until_commit() {
    let dir = Directory::new();
    for &existing in ATOMIC_CASES {
        let path = dir.0.join(if existing { "existing" } else { "new" });
        if existing {
            fs::write(&path, b"old preferences").expect("test precondition");
        }
        let staged =
            Replacement::prepare(&path, b"complete new preferences").expect("test precondition");
        if existing {
            assert_eq!(
                fs::read(&path).expect("test precondition"),
                b"old preferences"
            );
        } else {
            assert!(!path.try_exists().expect("stat config"));
        }
        assert_eq!(
            fs::read(staged.temporary()).expect("test precondition"),
            b"complete new preferences"
        );
        staged.commit().expect("test precondition");
        assert_eq!(
            fs::read(&path).expect("test precondition"),
            b"complete new preferences"
        );
    }
    assert_eq!(
        fs::read_dir(&dir.0).expect("test precondition").count(),
        ATOMIC_CASES.len()
    );
}

#[test]
fn abandoned_and_failed_publication_leave_config_unchanged() {
    for &existing in ATOMIC_CASES {
        let dir = Directory::new();
        let path = dir.0.join("config");
        if existing {
            fs::write(&path, b"original").expect("test precondition");
        }
        drop(Replacement::prepare(&path, b"new").expect("test precondition"));
        let staged = Replacement::prepare(&path, b"new").expect("test precondition");
        fs::remove_file(staged.temporary()).expect("test precondition");
        assert_eq!(
            staged.commit().expect_err("test precondition").kind(),
            io::ErrorKind::NotFound
        );
        if existing {
            assert_eq!(fs::read(&path).expect("test precondition"), b"original");
        } else {
            assert!(!path.try_exists().expect("stat config"));
        }
        assert_eq!(
            fs::read_dir(&dir.0).expect("test precondition").count(),
            usize::from(existing)
        );
        assert!(write_config(&dir.0, b"not a file").is_err());
        assert!(fs::metadata(&dir.0).expect("stat directory").is_dir());
    }
}

#[test]
fn config_update_lock_covers_the_full_read_modify_write() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let env = IsolatedEnv::new();
    let state_home = env.path().join("state");
    env.set("XDG_STATE_HOME", &state_home);
    let dir = Directory::new();
    let path = dir.0.join("settings.json");
    fs::write(&path, "0").expect("test precondition");

    let start = std::sync::Arc::new(std::sync::Barrier::new(3));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let path = path.clone();
            let start = std::sync::Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                let _lock = lock_config_for_update(&path).expect("test precondition");
                let value = fs::read_to_string(&path)
                    .expect("test precondition")
                    .parse::<u32>()
                    .expect("test precondition");
                std::thread::sleep(std::time::Duration::from_millis(10));
                write_config(&path, (value + 1).to_string()).expect("test precondition");
            })
        })
        .collect();
    start.wait();
    for worker in workers {
        worker.join().expect("test precondition");
    }

    assert_eq!(fs::read_to_string(&path).expect("test precondition"), "2");
    assert_eq!(fs::read_dir(&dir.0).expect("test precondition").count(), 1);

    let target = resolve_target(&path).expect("test precondition");
    let lock_path = config_update_lock_path(&target).expect("test precondition");
    assert!(lock_path.starts_with(state_home));
    let metadata = fs::metadata(&lock_path).expect("persistent config lock");
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(
        metadata.uid(),
        fs::metadata(env.home()).expect("test precondition").uid()
    );
    let first_inode = metadata.ino();
    {
        let _lock = lock_config_for_update(&path).expect("test precondition");
    }
    assert_eq!(
        fs::metadata(&lock_path)
            .expect("persistent config lock")
            .ino(),
        first_inode
    );
}

#[test]
fn config_update_lock_ignores_empty_and_refuses_relative_state_home() {
    let env = IsolatedEnv::new();
    let dir = Directory::new();
    let target = dir.0.join("settings.json");
    let default_dir = env.home().join(".local/state/shepr/integration-locks");
    env.set("XDG_STATE_HOME", "");
    let lock_path = config_update_lock_path(&target).expect("test precondition");
    assert!(lock_path.starts_with(&default_dir), "{lock_path:?}");

    env.set("XDG_STATE_HOME", "relative/state");
    let error = config_update_lock_path(&target).expect_err("a relative state home is refused");
    assert!(error.to_string().contains("XDG_STATE_HOME"), "{error}");
}

#[test]
fn config_update_lock_that_cannot_be_created_fails_the_edit() {
    let env = IsolatedEnv::new();
    // A regular file where the state directory should be: the lock directory
    // cannot be created, and the edit must not go ahead unlocked.
    let blocker = env.path().join("state-file");
    fs::write(&blocker, "").expect("test precondition");
    env.set("XDG_STATE_HOME", &blocker);
    let dir = Directory::new();
    let path = dir.0.join("settings.json");

    let error = match lock_config_for_update(&path) {
        Ok(_) => panic!("an uncreatable lock directory must fail the edit"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("could not lock"),
        "unexpected error: {error}"
    );
}

#[test]
fn config_update_lock_resolves_parent_symlink_aliases() {
    let env = IsolatedEnv::new();
    let state_home = env.path().join("state");
    env.set("XDG_STATE_HOME", &state_home);
    let dir = Directory::new();
    let actual = dir.0.join("actual");
    fs::create_dir(&actual).expect("test precondition");
    let alias = dir.0.join("alias");
    std::os::unix::fs::symlink(&actual, &alias).expect("test precondition");

    let alias_config = alias.join("nested/settings.json");
    let actual_config = actual.join("nested/settings.json");
    let alias_target = resolve_target(&alias_config).expect("test precondition");
    let actual_target = resolve_target(&actual_config).expect("test precondition");
    let alias_lock = config_update_lock_path(&alias_target).expect("test precondition");
    let actual_lock = config_update_lock_path(&actual_target).expect("test precondition");
    assert_eq!(alias_lock, actual_lock);

    let _lock = lock_config_for_update(&alias_config).expect("test precondition");
    let error = match shepr_platform::ipc::acquire_flock_lock(&actual_lock, false) {
        Ok(_) => panic!("alias path did not share the existing config lock"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(fs::read_dir(&actual).expect("test precondition").count(), 0);
}

#[test]
fn hard_links_are_rejected_before_staging_and_rechecked_before_commit() {
    let dir = Directory::new();
    let path = dir.0.join("config");
    let alias = dir.0.join("alias");
    fs::write(&path, b"original").expect("test precondition");
    #[cfg(unix)]
    let staged = Replacement::prepare(&path, b"new").expect("test precondition");
    fs::hard_link(&path, &alias).expect("test precondition");
    #[cfg(unix)]
    assert!(
        staged
            .commit()
            .expect_err("test precondition")
            .to_string()
            .contains("multiple hard links")
    );
    for candidate in [&path, &alias] {
        let error = write_config(candidate, b"new")
            .expect_err("test precondition")
            .to_string();
        assert!(error.contains(&candidate.display().to_string()));
        assert_eq!(fs::read(candidate).expect("test precondition"), b"original");
        assert_eq!(
            shepr_platform::config_file_link_count(candidate).expect("test precondition"),
            2
        );
    }
    assert_eq!(fs::read_dir(&dir.0).expect("test precondition").count(), 2);
}

#[test]
fn symlink_chains_and_dangling_targets_preserve_links() {
    let dir = Directory::new();
    let other = dir.0.join("other");
    fs::create_dir(&other).expect("test precondition");
    let target = other.join("preferences");
    let intermediate = other.join("link");
    let entry = dir.0.join("config");
    symlink_file(&target, &intermediate);
    let relative_target = Path::new("other").join("link");
    symlink_file(&relative_target, &entry);
    let original_intermediate = fs::read_link(&intermediate).expect("test precondition");
    assert_eq!(
        fs::metadata(&entry).expect_err("test precondition").kind(),
        io::ErrorKind::NotFound
    );
    write_config(&entry, b"first install").expect("test precondition");
    write_config(&entry, b"second install").expect("test precondition");
    assert_eq!(
        fs::read(&target).expect("test precondition"),
        b"second install"
    );
    assert_eq!(
        fs::read_link(&entry).expect("test precondition"),
        relative_target
    );
    assert_eq!(
        fs::read_link(&intermediate).expect("test precondition"),
        original_intermediate
    );
    assert_eq!(fs::read_dir(&other).expect("test precondition").count(), 2);
    let alias = other.join("hard-link");
    fs::hard_link(&target, &alias).expect("test precondition");
    assert!(write_config(&entry, b"must not change").is_err());
    assert_eq!(
        fs::read(&alias).expect("test precondition"),
        b"second install"
    );
    assert_eq!(
        fs::read_link(&entry).expect("test precondition"),
        relative_target
    );

    let cycle = dir.0.join("cycle");
    symlink_file(Path::new("cycle"), &cycle);
    assert!(write_config(&cycle, b"must not replace the link").is_err());
    assert_eq!(
        fs::read_link(&cycle).expect("test precondition"),
        Path::new("cycle")
    );
}

#[test]
fn existing_permissions_and_new_file_defaults_are_preserved() {
    let dir = Directory::new();
    let path = dir.0.join("existing");
    fs::write(&path, b"original").expect("test precondition");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("test precondition");
    }
    let permissions = fs::metadata(&path)
        .expect("test precondition")
        .permissions();
    write_config(&path, b"new").expect("test precondition");
    assert_eq!(
        fs::metadata(&path)
            .expect("test precondition")
            .permissions(),
        permissions
    );
    let ordinary = dir.0.join("ordinary");
    let new = dir.0.join("new");
    fs::write(&ordinary, b"ordinary creation").expect("test precondition");
    write_config(&new, b"atomic creation").expect("test precondition");
    assert_eq!(
        fs::metadata(&ordinary)
            .expect("test precondition")
            .permissions(),
        fs::metadata(&new).expect("test precondition").permissions()
    );
}

#[cfg(unix)]
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "AGENT_TEST_CONFIG_READ_ONLY_PATH is this test's own re-exec harness probe, not a shepr setting"
)]
fn writable_directory_does_not_bypass_read_only_config() {
    use std::os::unix::{fs::PermissionsExt, process::CommandExt};
    const CHILD: &str = "AGENT_TEST_CONFIG_READ_ONLY_PATH";
    if let Some(path) = std::env::var_os(CHILD) {
        let error =
            write_config(Path::new(&path), b"must not replace").expect_err("test precondition");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        println!("read-only rejection executed");
        return;
    }
    let dir = Directory::new();
    let path = dir.0.join("read-only");
    fs::write(&path, b"original").expect("test precondition");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).expect("test precondition");
    fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o777)).expect("test precondition");
    let mut child = shepr_test_support::command_in_scratch(
        std::env::current_exe().expect("test precondition"),
        "config-read-only-child",
    );
    child
        .args([
            "--exact",
            "integration::config_file::tests::writable_directory_does_not_bypass_read_only_config",
            "--nocapture",
        ])
        .env(CHILD, &path);
    // Root bypasses Unix mode checks through two capabilities. A root child
    // drops them from its bounding set before exec, so the exec's re-grant of
    // root's capabilities leaves them out and the mode bits apply. It keeps
    // root's identity, so this test's private (0700) scratch stays reachable,
    // which a switch to an unprivileged uid would lose.
    // SAFETY: geteuid takes no arguments, cannot fail and touches no memory.
    if unsafe { libc::geteuid() } == 0 {
        const CAP_DAC_OVERRIDE: libc::c_ulong = 1;
        const CAP_DAC_READ_SEARCH: libc::c_ulong = 2;
        // SAFETY: the hook runs in the forked child before exec and makes only
        // prctl system calls, which are async-signal-safe and allocate nothing.
        unsafe {
            child.pre_exec(|| {
                for capability in [CAP_DAC_OVERRIDE, CAP_DAC_READ_SEARCH] {
                    if libc::prctl(libc::PR_CAPBSET_DROP, capability, 0, 0, 0) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
    let output = child.output().expect("test precondition");
    assert!(output.status.success(), "child failed: {output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("read-only rejection executed"));
    assert_eq!(fs::read(&path).expect("test precondition"), b"original");
    assert_eq!(fs::read_dir(&dir.0).expect("test precondition").count(), 1);
}

#[cfg(unix)]
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "AGENT_TEST_CONFIG_PARTIAL_WRITE_DIR is this test's own re-exec harness probe, not a shepr setting"
)]
fn partial_write_errors_preserve_files_and_do_not_remove_collisions() {
    const CHILD: &str = "AGENT_TEST_CONFIG_PARTIAL_WRITE_DIR";
    if let Some(path) = std::env::var_os(CHILD) {
        let dir = PathBuf::from(path);
        // This process runs only this test. A collision must neither be used nor removed.
        crate::integration::atomic_replace::reset_temp_sequence(0);
        crate::integration::atomic_replace::set_temp_token_for_test(0);
        let collision =
            crate::integration::atomic_replace::temporary_path_for_test(&dir, ".shepr-config", 0)
                .expect("test precondition");
        fs::write(&collision, b"unrelated file").expect("test precondition");
        for name in ["existing", "new"] {
            let error =
                write_config(&dir.join(name), vec![b'x'; 8192]).expect_err("test precondition");
            assert_eq!(error.raw_os_error(), Some(libc::EFBIG));
        }
        assert_eq!(
            fs::read(collision).expect("test precondition"),
            b"unrelated file"
        );
        println!("partial-write paths executed");
        return;
    }
    let dir = Directory::new();
    fs::write(dir.0.join("existing"), b"original").expect("test precondition");
    // A one-kibibyte limit, which the 8 KiB writes cross partway. Ignoring
    // SIGXFSZ makes the kernel return EFBIG instead of killing the child; the
    // ignored disposition and the limit both survive the exec.
    use shepr_test_support::fixture::{self, Signal, Step};
    let output = fixture::command(&[
        Step::Ignore(Signal::Xfsz),
        Step::LimitFileSize(1024),
        Step::Exec(vec![
            std::env::current_exe().expect("test precondition").into(),
            "--exact".into(),
            "integration::config_file::tests::partial_write_errors_preserve_files_and_do_not_remove_collisions".into(),
            "--nocapture".into(),
        ]),
    ])
    .env(CHILD, &dir.0)
    .output()
    .expect("test precondition");
    assert!(output.status.success(), "child failed: {output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("partial-write paths executed"));
    assert_eq!(
        fs::read(dir.0.join("existing")).expect("test precondition"),
        b"original"
    );
    assert!(!dir.0.join("new").try_exists().expect("stat new"));
    assert_eq!(
        fs::read_dir(&dir.0).expect("test precondition").count(),
        2,
        "only original and collision may remain"
    );
}
