use super::*;
use crate::integration::test_support::symlink_file;
use shepr_test_support::IsolatedEnv;

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        Self(shepr_test_support::ScratchDir::new("config-write").keep_until_exit())
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
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
            assert!(!path.exists());
        }
        assert_eq!(
            fs::read(&staged.temporary).expect("test precondition"),
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
        fs::remove_file(&staged.temporary).expect("test precondition");
        assert_eq!(
            staged.commit().expect_err("test precondition").kind(),
            io::ErrorKind::NotFound
        );
        if existing {
            assert_eq!(fs::read(&path).expect("test precondition"), b"original");
        } else {
            assert!(!path.exists());
        }
        assert_eq!(
            fs::read_dir(&dir.0).expect("test precondition").count(),
            usize::from(existing)
        );
        assert!(write_config(&dir.0, b"not a file").is_err());
        assert!(dir.0.is_dir());
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
fn config_update_lock_ignores_empty_or_relative_state_home() {
    let env = IsolatedEnv::new();
    let dir = Directory::new();
    let target = dir.0.join("settings.json");
    let default_dir = env.home().join(".local/state/shepr/integration-locks");
    for value in ["", "relative/state"] {
        env.set("XDG_STATE_HOME", value);
        let lock_path = config_update_lock_path(&target).expect("test precondition");
        assert!(
            lock_path.starts_with(&default_dir),
            "{value:?}: {lock_path:?}"
        );
    }
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
    if !symlink_file(&target, &intermediate) {
        return;
    }
    let relative_target = Path::new("other").join("link");
    assert!(symlink_file(&relative_target, &entry));
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
    assert!(symlink_file(Path::new("cycle"), &cycle));
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
fn writable_directory_does_not_bypass_read_only_config() {
    use std::os::unix::{fs::PermissionsExt, process::CommandExt};
    const CHILD: &str = "SHEPR_CONFIG_READ_ONLY_TEST";
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
    let mut child = std::process::Command::new(std::env::current_exe().expect("test precondition"));
    child
        .args([
            "--exact",
            "integration::config_file::tests::writable_directory_does_not_bypass_read_only_config",
            "--nocapture",
        ])
        .env(CHILD, &path);
    // Root bypasses Unix mode checks. Test the real user path in a child instead.
    if unsafe { libc::geteuid() } == 0 {
        child.gid(65534).uid(65534);
    }
    let output = child.output().expect("test precondition");
    assert!(output.status.success(), "child failed: {output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("read-only rejection executed"));
    assert_eq!(fs::read(&path).expect("test precondition"), b"original");
    assert_eq!(fs::read_dir(&dir.0).expect("test precondition").count(), 1);
}

#[cfg(unix)]
#[test]
fn partial_write_errors_preserve_files_and_do_not_remove_collisions() {
    const CHILD: &str = "SHEPR_CONFIG_PARTIAL_WRITE_TEST";
    if let Some(path) = std::env::var_os(CHILD) {
        let dir = PathBuf::from(path);
        // This process runs only this test. A collision must neither be used nor removed.
        NEXT_TEMP.store(0, Ordering::Relaxed);
        let collision = dir.join(format!(".shepr-config-{}-0.tmp", std::process::id()));
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
    let output = std::process::Command::new("bash")
        .args(["-c", "trap '' XFSZ; ulimit -f 1; exec \"$@\"", "shepr-test"])
        .arg(std::env::current_exe().expect("test precondition"))
        .args(["--exact", "integration::config_file::tests::partial_write_errors_preserve_files_and_do_not_remove_collisions", "--nocapture"])
        .env(CHILD, &dir.0)
        .output().expect("test precondition");
    assert!(output.status.success(), "child failed: {output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("partial-write paths executed"));
    assert_eq!(
        fs::read(dir.0.join("existing")).expect("test precondition"),
        b"original"
    );
    assert!(!dir.0.join("new").exists());
    assert_eq!(
        fs::read_dir(&dir.0).expect("test precondition").count(),
        2,
        "only original and collision may remain"
    );
}
