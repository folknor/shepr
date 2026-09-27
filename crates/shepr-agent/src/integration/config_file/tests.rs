use super::*;
use crate::integration::test_support::symlink_file;

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
