use super::*;
use shepr_core::socket_path::fits_unix_socket_path;

#[test]
fn ssh_control_path_rejects_percent_tokens_in_runtime_directory() {
    for runtime_dir in [Path::new("/run/%h"), Path::new("/run/%%")] {
        let error = ssh_control_path_under(
            runtime_dir,
            Path::new("/config/one"),
            SshControlKey::from_identity_bytes(b"host"),
        )
        .expect_err("OpenSSH would reinterpret percent sequences in ControlPath");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("percent sequences"));
    }
}

#[test]
fn bridge_socket_names_carry_a_random_token_before_the_extension() {
    assert_eq!(
        with_name_token("shepr-r-42-dev.sock", 0xab),
        "shepr-r-42-dev.00000000000000ab.sock"
    );
    assert_eq!(with_name_token("bridge", 1), "bridge.0000000000000001");
    assert_eq!(with_name_token(".sock", 1), ".sock.0000000000000001");

    let runtime_dir = shepr_test_support::ScratchDir::new("bridge-endpoints");
    let first =
        remote_bridge_endpoint_path(runtime_dir.path(), "shepr-t-1-a.sock", "shepr-t-1.sock")
            .expect("test precondition");
    let second =
        remote_bridge_endpoint_path(runtime_dir.path(), "shepr-t-1-a.sock", "shepr-t-1.sock")
            .expect("test precondition");
    assert_ne!(
        first, second,
        "concurrent bridges must receive distinct socket paths"
    );
    for path in [&first, &second] {
        assert!(fits_unix_socket_path(path), "{}", path.display());
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        assert!(name.starts_with("shepr-t-1"), "{name}");
        assert!(name.ends_with(".sock"), "{name}");
    }
}

#[test]
fn remote_ssh_config_dir_is_private_and_under_the_runtime_directory() {
    use std::os::unix::fs::PermissionsExt;

    let runtime_dir = shepr_test_support::ScratchDir::new("ssh-config-runtime");
    let first = create_remote_ssh_config_dir(runtime_dir.path()).expect("test precondition");
    let second = create_remote_ssh_config_dir(runtime_dir.path()).expect("test precondition");
    assert!(first.starts_with(runtime_dir.path()));
    assert_ne!(first, second);
    assert_eq!(
        std::fs::metadata(&first)
            .expect("test precondition")
            .permissions()
            .mode()
            & 0o777,
        shepr_platform::PRIVATE_DIRECTORY_MODE
    );
    std::fs::write(remote_ssh_config_file_path(&first), "Host *\n").expect("test precondition");
    release_remote_ssh_config_dir(&first);
    release_remote_ssh_config_dir(&first);
    assert!(!first.try_exists().expect("stat released directory"));
}

#[test]
fn shared_ssh_control_path_is_stable_scoped_and_bounded() {
    // The control socket's name and OpenSSH's staging suffix leave room only
    // for a runtime directory as short as a real one, which no scratch
    // directory under the build tree is; the naming and length arithmetic are
    // exercised over the real directory's spelling, and the directory checks
    // that `shared_ssh_control_path` adds are covered below.
    let runtime_dir = Path::new("/run/user/4294967294");
    let path = ssh_control_path_under(
        runtime_dir,
        Path::new("/config/one"),
        SshControlKey::from_identity_bytes(b"user@host"),
    )
    .expect("test precondition");
    assert_eq!(path.parent(), Some(runtime_dir));
    assert_eq!(
        path,
        ssh_control_path_under(
            runtime_dir,
            Path::new("/config/one"),
            SshControlKey::from_identity_bytes(b"user@host")
        )
        .expect("test precondition")
    );
    assert_ne!(
        path,
        ssh_control_path_under(
            runtime_dir,
            Path::new("/config/two"),
            SshControlKey::from_identity_bytes(b"user@host")
        )
        .expect("test precondition")
    );
    assert_ne!(
        path,
        ssh_control_path_under(
            runtime_dir,
            Path::new("/config/one"),
            SshControlKey::from_identity_bytes(b"other@host")
        )
        .expect("test precondition")
    );
    let expanded = path.to_string_lossy().replace("%C", &"f".repeat(40));
    assert!(fits_unix_socket_path(&PathBuf::from(&expanded)));
    // OpenSSH binds this temporary socket before renaming it to ControlPath.
    assert!(fits_unix_socket_path(&PathBuf::from(format!(
        "{expanded}.QuuYe7ZFE2HYeAE4"
    ))));
}

#[test]
fn shared_ssh_control_path_validates_the_runtime_directory_first() {
    use std::os::unix::fs::PermissionsExt;

    let scratch = shepr_test_support::ScratchDir::new("ssh-control-unsafe-runtime");
    let runtime_dir = scratch.join("runtime");
    std::fs::create_dir(&runtime_dir).expect("test precondition");
    std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o755))
        .expect("test precondition");
    let error = shared_ssh_control_path(
        &runtime_dir,
        Path::new("/config/one"),
        SshControlKey::from_identity_bytes(b"user@host"),
    )
    .expect_err("a runtime directory others can reach is refused");
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(
        error
            .to_string()
            .contains(&runtime_dir.display().to_string())
    );
    assert!(matches!(error, SshRuntimeError::UnsafeDirectory(_)));
    let relative = shared_ssh_control_path(
        Path::new("relative/runtime"),
        Path::new("/config/one"),
        SshControlKey::from_identity_bytes(b"user@host"),
    )
    .expect_err("a relative runtime directory is refused");
    assert_eq!(relative.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn shared_ssh_control_path_rejects_a_runtime_dir_that_cannot_fit_open_ssh_staging() {
    use std::os::unix::fs::PermissionsExt;

    let scratch = shepr_test_support::ScratchDir::new("ssh-control-long-runtime");
    let runtime_dir = scratch.path().join("x".repeat(90));
    std::fs::create_dir(&runtime_dir).expect("test precondition");
    std::fs::set_permissions(
        &runtime_dir,
        std::fs::Permissions::from_mode(shepr_platform::PRIVATE_DIRECTORY_MODE),
    )
    .expect("test precondition");
    let error = shared_ssh_control_path(
        &runtime_dir,
        Path::new("/config/one"),
        SshControlKey::from_identity_bytes(b"user@host"),
    )
    .expect_err("the OpenSSH staging path must fit");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn shared_ssh_directory_rejects_symlinks_and_public_modes() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let runtime_dir = shepr_test_support::ScratchDir::new("ssh-directory-validation");
    let dir = create_remote_ssh_config_dir(runtime_dir.path()).expect("test precondition");
    let link = dir.join("link");
    symlink(&dir, &link).expect("test precondition");
    assert_eq!(
        validate_shared_ssh_dir(&link)
            .expect_err("test precondition")
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert!(matches!(
        validate_shared_ssh_dir(&link).expect_err("test precondition"),
        SshRuntimeError::UnsafeDirectory(_)
    ));
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
        .expect("test precondition");
    assert_eq!(
        validate_shared_ssh_dir(&dir)
            .expect_err("test precondition")
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
}
