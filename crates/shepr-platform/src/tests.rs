use super::*;
use std::{
    io::Read,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

// ---------------------------------------------------------------------------
// Child exit, raw reads, terminal
// ---------------------------------------------------------------------------

#[test]
fn child_exit_classification_only_checkpoints_interruptions() {
    use std::os::unix::process::ExitStatusExt;

    for code in [0, 1, 130, 255] {
        // Raw wait status: exit code in bits 8..16, no terminating signal.
        let reason = classify_child_exit(&std::process::ExitStatus::from_raw(code << 8));
        assert_eq!(reason, ChildExitReason::Exited, "exit code {code:#x}");
        assert!(!reason.requires_session_checkpoint());
    }
    let status = std::process::ExitStatus::from_raw(libc::SIGTERM);
    assert_eq!(classify_child_exit(&status), ChildExitReason::Interrupted);
    assert!(classify_child_exit(&status).requires_session_checkpoint());
    assert!(!ChildExitReason::WaitFailed.requires_session_checkpoint());
}

#[test]
fn terminal_resize_signal_is_recorded_once_per_delivery() {
    watch_terminal_resize_signal();
    assert!(!take_terminal_resize_signal());

    // SAFETY: raise(3) delivers SIGWINCH to this thread; the handler that
    // `watch_terminal_resize_signal` installed only stores an atomic.
    unsafe {
        libc::raise(libc::SIGWINCH);
    }

    assert!(take_terminal_resize_signal());
    assert!(!take_terminal_resize_signal());
}

#[test]
fn read_limited_reader_returns_complete_data_under_limit() {
    let input = std::io::Cursor::new(b"image".to_vec());
    assert_eq!(
        read_limited_reader(input, 16).expect("limited read"),
        LimitedRead::Complete(b"image".to_vec())
    );
}

#[test]
fn read_limited_reader_returns_empty_for_empty_input() {
    let input = std::io::Cursor::new(Vec::<u8>::new());
    assert_eq!(
        read_limited_reader(input, 16).expect("limited read"),
        LimitedRead::Empty
    );
}

#[test]
fn read_limited_reader_accepts_data_exactly_at_limit() {
    let input = std::io::Cursor::new(b"four".to_vec());
    assert_eq!(
        read_limited_reader(input, 4).expect("limited read"),
        LimitedRead::Complete(b"four".to_vec())
    );
}

#[test]
fn read_limited_reader_rejects_data_over_limit() {
    let input = std::io::Cursor::new(b"oversized".to_vec());
    assert_eq!(
        read_limited_reader(input, 4).expect("limited read"),
        LimitedRead::Oversized
    );
}

#[test]
fn read_limited_reader_retries_interrupted_reads() {
    struct InterruptedOnce {
        interrupted: bool,
        inner: std::io::Cursor<Vec<u8>>,
    }

    impl Read for InterruptedOnce {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            self.inner.read(buffer)
        }
    }

    let input = InterruptedOnce {
        interrupted: false,
        inner: std::io::Cursor::new(b"image".to_vec()),
    };
    assert_eq!(
        read_limited_reader(input, 16).expect("limited read"),
        LimitedRead::Complete(b"image".to_vec())
    );
}

#[test]
fn wsl_marker_detection_matches_kernel_release_text() {
    assert!(text_indicates_wsl("5.15.167.4-microsoft-standard-WSL2"));
    assert!(text_indicates_wsl("4.4.0-19041-Microsoft"));
    assert!(!text_indicates_wsl("6.8.0-64-generic"));
    assert!(!text_indicates_wsl(""));
}

#[test]
fn proc_stat_yields_session_and_controlling_tty() {
    assert_eq!(
        session_and_tty_from_stat("4242 (shepr (srv) x) S 1 4242 4242 0 -1 4194560"),
        Some((4242, 0))
    );
    assert_eq!(
        session_and_tty_from_stat("77 (shepr) S 70 77 77 34817 77 4194560"),
        Some((77, 34817))
    );
    assert_eq!(session_and_tty_from_stat("77 (shepr) S 70"), None);
}

#[test]
fn only_a_session_leader_without_a_terminal_counts_as_detached() {
    // The setsid daemon spawn: leads its session, no controlling tty.
    assert!(is_detached_session(4242, 4242, 0));
    // `terminal -e shepr server` or `ssh -t host shepr server`: a session
    // leader too, but the terminal is its controlling tty.
    assert!(!is_detached_session(77, 77, 34817));
    // A server started from an interactive shell belongs to the shell's
    // session.
    assert!(!is_detached_session(90, 70, 34817));
    assert!(!is_detached_session(90, 70, 0));
}

#[test]
fn bridge_socket_names_carry_a_random_token_before_the_extension() {
    assert_eq!(
        with_name_token("shepr-r-42-dev.sock", 0xab),
        "shepr-r-42-dev.00000000000000ab.sock"
    );
    assert_eq!(with_name_token("bridge", 1), "bridge.0000000000000001");
    assert_eq!(with_name_token(".sock", 1), ".sock.0000000000000001");

    let first = remote_bridge_endpoint_path("shepr-t-1-a.sock", "shepr-t-1.sock");
    let second = remote_bridge_endpoint_path("shepr-t-1-a.sock", "shepr-t-1.sock");
    assert_ne!(
        first, second,
        "a squatter must not be able to predict the path"
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
fn launch_executable_follows_a_replaced_binary_to_its_new_install() {
    let installed = |path: &Path| path == Path::new("/usr/bin/shepr");
    // A running binary that an install replaced.
    assert_eq!(
        resolve_launch_executable(PathBuf::from("/usr/bin/shepr (deleted)"), installed),
        PathBuf::from("/usr/bin/shepr")
    );
    // The normal case: the path is there, nothing is rewritten.
    assert_eq!(
        resolve_launch_executable(PathBuf::from("/usr/bin/shepr"), installed),
        PathBuf::from("/usr/bin/shepr")
    );
    // Removed with no replacement: keep the reported path, there is nothing
    // better to offer.
    assert_eq!(
        resolve_launch_executable(PathBuf::from("/opt/shepr (deleted)"), installed),
        PathBuf::from("/opt/shepr (deleted)")
    );
    // A binary whose real name ends in the suffix is left alone.
    let literal = |path: &Path| path == Path::new("/opt/shepr (deleted)");
    assert_eq!(
        resolve_launch_executable(PathBuf::from("/opt/shepr (deleted)"), literal),
        PathBuf::from("/opt/shepr (deleted)")
    );
}

// ---------------------------------------------------------------------------
// SSH paths
// ---------------------------------------------------------------------------

#[test]
fn unix_socket_paths_may_use_the_whole_linux_limit() {
    assert!(fits_unix_socket_path(Path::new(&"x".repeat(107))));
    assert!(!fits_unix_socket_path(Path::new(&"x".repeat(108))));
}

#[test]
fn remote_ssh_config_dir_rejects_overlong_control_socket_name() {
    let err = create_remote_ssh_config_dir(&"x".repeat(200)).expect_err("test precondition");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn shared_ssh_control_path_is_stable_scoped_and_bounded() {
    let path =
        shared_ssh_control_path(Path::new("/config/one"), "user@host").expect("test precondition");
    assert_eq!(
        path,
        shared_ssh_control_path(Path::new("/config/one"), "user@host").expect("test precondition")
    );
    assert_ne!(
        path,
        shared_ssh_control_path(Path::new("/config/two"), "user@host").expect("test precondition")
    );
    assert_ne!(
        path,
        shared_ssh_control_path(Path::new("/config/one"), "other@host").expect("test precondition")
    );
    let expanded = path.to_string_lossy().replace("%C", &"f".repeat(40));
    assert!(fits_unix_socket_path(&PathBuf::from(&expanded)));
    // OpenSSH binds this temporary socket before renaming it to ControlPath.
    assert!(fits_unix_socket_path(&PathBuf::from(format!(
        "{expanded}.QuuYe7ZFE2HYeAE4"
    ))));
    validate_shared_ssh_dir(path.parent().expect("test precondition")).expect("test precondition");
}

#[test]
fn shared_ssh_staging_path_fits_with_maximum_uid_width() {
    let path =
        shared_ssh_control_path(Path::new("/config/one"), "user@host").expect("test precondition");
    let directory = path.parent().expect("test precondition");
    let name = directory
        .file_name()
        .expect("test precondition")
        .to_string_lossy();
    let prefix = name.trim_end_matches(|ch: char| ch.is_ascii_digit());
    let maximum_uid_directory = directory
        .parent()
        .expect("test precondition")
        .join(format!("{prefix}{}", u32::MAX));
    let expanded = maximum_uid_directory
        .join(path.file_name().expect("test precondition"))
        .to_string_lossy()
        .replace("%C", &"f".repeat(40));
    assert!(fits_unix_socket_path(&PathBuf::from(format!(
        "{expanded}.QuuYe7ZFE2HYeAE4"
    ))));
}

#[test]
fn shared_ssh_directory_rejects_symlinks_and_public_modes() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = create_remote_ssh_config_dir("ctl").expect("test precondition");
    let link = dir.join("link");
    symlink(&dir, &link).expect("test precondition");
    assert_eq!(
        validate_shared_ssh_dir(&link)
            .expect_err("test precondition")
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
        .expect("test precondition");
    assert_eq!(
        validate_shared_ssh_dir(&dir)
            .expect_err("test precondition")
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
    std::fs::remove_dir_all(dir).expect("test precondition");
}

// ---------------------------------------------------------------------------
// Config file replacement
// ---------------------------------------------------------------------------

fn set_attribute(file: &std::fs::File, name: &std::ffi::CStr, value: &[u8]) {
    assert_eq!(
        // SAFETY: `name` is NUL-terminated; fsetxattr reads `value.len()`
        // bytes from the live slice.
        unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        },
        0,
        "{}",
        std::io::Error::last_os_error()
    );
}

fn attribute(file: &std::fs::File, name: &std::ffi::CStr) -> Option<Vec<u8>> {
    let mut value = vec![0; 1024];
    // SAFETY: `name` is NUL-terminated; fgetxattr writes at most
    // `value.len()` bytes into the live buffer.
    let read = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    let Ok(read) = usize::try_from(read) else {
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ENODATA)
        );
        return None;
    };
    value.truncate(read);
    Some(value)
}

#[test]
fn config_metadata_preserves_ownership_and_acl_without_inheriting_extra_access() {
    use std::os::unix::fs::MetadataExt;

    // Keep this ACL-specific probe on the repository filesystem so it can
    // exercise POSIX ACL xattrs independently of the system temp mount.
    let target = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = shepr_test_support::ScratchDir::new_in(target, "config-acl");
    // Linux UAPI posix_acl_xattr_header/entry, version 2, little-endian fields.
    // The test runner maps only its current uid, so use that id for the named
    // entry instead of an unmapped uid that the kernel rejects with EINVAL.
    // Owner rw, named user read, group none, mask read, other none.
    let named_user = effective_uid();
    let mut acl = 2_u32.to_le_bytes().to_vec();
    for (tag, permissions, id) in [
        (1_u16, 6_u16, u32::MAX),
        (2, 4, named_user),
        (4, 0, u32::MAX),
        (16, 4, u32::MAX),
        (32, 0, u32::MAX),
    ] {
        acl.extend(tag.to_le_bytes());
        acl.extend(permissions.to_le_bytes());
        acl.extend(id.to_le_bytes());
    }
    for has_acl in [false, true] {
        let source = dir.join(format!("source-{has_acl}"));
        let target = dir.join(format!("target-{has_acl}"));
        std::fs::write(&source, b"old").expect("test precondition");
        let input = std::fs::File::open(&source).expect("test precondition");
        if effective_uid() == 0 {
            // SAFETY: fchown(2) on an fd `input` keeps open; integers only.
            assert_eq!(unsafe { libc::fchown(input.as_raw_fd(), 1001, 1002) }, 0);
        }
        if has_acl {
            set_attribute(&input, c"system.posix_acl_access", &acl);
        }
        set_attribute(&input, c"user.shepr-test", b"preserve this attribute");
        let original = input.metadata().expect("test precondition");
        drop(create_private_temporary(&target).expect("test precondition"));
        let output = std::fs::File::open(&target).expect("test precondition");
        // Model a default ACL inherited from the destination's parent directory.
        set_attribute(&output, c"system.posix_acl_access", &acl);
        write_config_temporary(Some(&source), &target, b"new").expect("test precondition");
        let actual = output.metadata().expect("test precondition");
        assert_eq!(
            (actual.uid(), actual.gid(), actual.mode()),
            (original.uid(), original.gid(), original.mode())
        );
        assert_eq!(
            attribute(&output, c"system.posix_acl_access"),
            attribute(&input, c"system.posix_acl_access")
        );
        assert_eq!(
            attribute(&output, c"user.shepr-test"),
            Some(b"preserve this attribute".to_vec())
        );
        assert_eq!(std::fs::read(source).expect("test precondition"), b"old");
        assert_eq!(std::fs::read(target).expect("test precondition"), b"new");
    }
}

// ---------------------------------------------------------------------------
// Process handles and session teardown
// ---------------------------------------------------------------------------

#[test]
fn proc_stat_yields_state_and_start_time() {
    // Fields 3..=22 of stat(5); starttime (22) is 987654.
    let stat = "42 (a (b) c) S 1 42 42 0 -1 4194560 10 0 0 0 1 2 0 0 20 0 1 0 987654 1000 20";
    assert_eq!(state_and_start_time_from_stat(stat), Some(('S', 987_654)));
    assert_eq!(state_and_start_time_from_stat("42 (x) Z 1 42"), None);
}

/// Open handles either way: through a pidfd, and through the start-time
/// fallback used when the kernel has no pidfds.
fn both_handle_kinds(pid: u32) -> [ProcessHandle; 2] {
    let pidfd = ProcessHandle::open(pid).expect("pidfd_open on a live child");
    assert!(pidfd.pidfd().is_some(), "this kernel has pidfds");
    let fallback = ProcessHandle::open_by_start_time(pid).expect("stat of a live child");
    assert!(fallback.pidfd().is_none());
    [pidfd, fallback]
}

#[test]
fn process_handle_follows_one_process_through_exit_and_reap() {
    for kind in 0..2 {
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let handles = both_handle_kinds(child.id());
        let handle = &handles[kind];
        assert_eq!(handle.pid(), child.id());
        assert!(handle.is_unreaped());
        assert!(!handle.has_exited());

        assert!(handle.signal(Signal::Kill));
        assert!(wait_for_process_exits(&[handle], Duration::from_secs(5)));
        // A zombie has exited but still holds its pid.
        assert!(handle.has_exited());
        assert!(handle.is_unreaped());

        child.wait().expect("reap sleep");
        assert!(!handle.is_unreaped(), "handle kind {kind}");
        assert!(
            !handle.signal(Signal::Kill),
            "a reaped process's handle must not signal anything (kind {kind})"
        );
    }
}

#[test]
fn wait_for_process_exits_times_out_on_a_live_process() {
    let mut child = Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep");
    for handle in &both_handle_kinds(child.id()) {
        assert!(!wait_for_process_exits(
            &[handle],
            Duration::from_millis(30)
        ));
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn spawn_session_with_background_job() -> std::process::Child {
    use std::os::unix::process::CommandExt as _;

    let mut command = Command::new("/bin/sh");
    command.args(["-c", "sleep 30 & exec sleep 30"]);
    // SAFETY: setsid has no Rust memory preconditions in the single-threaded child.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().expect("spawn session")
}

#[test]
fn session_members_are_found_without_the_leader_and_signalled_by_handle() {
    // Once through pidfds, once through the start-time fallback a kernel
    // without pidfds gets: background jobs must be reached either way.
    let openers: [fn(u32) -> Option<ProcessHandle>; 2] =
        [ProcessHandle::open, ProcessHandle::open_by_start_time];
    for open in openers {
        let mut child = spawn_session_with_background_job();
        let leader = child.id();
        let deadline = Instant::now() + Duration::from_secs(5);
        let members = loop {
            let members = session_member_handles_with(leader, || false, open);
            if !members.is_empty() || Instant::now() >= deadline {
                break members;
            }
            std::thread::sleep(Duration::from_millis(10));
        };

        assert!(
            members.iter().all(|member| member.pid() != leader),
            "the leader is not a member handle"
        );
        assert_eq!(members.len(), 1, "the background sleep is the only member");
        for member in &members {
            assert!(member.signal(Signal::Kill));
        }
        let _ = child.kill();
        let _ = child.wait();
        let handles: Vec<&ProcessHandle> = members.iter().collect();
        assert!(wait_for_process_exits(&handles, Duration::from_secs(5)));
    }
}

#[test]
fn session_members_are_withheld_when_a_reaped_leaders_pid_is_held_again() {
    let mut child = spawn_session_with_background_job();
    let leader = child.id();
    // The leader is alive, so from the point of view of a caller that has
    // already reaped its own leader, pid `leader` belongs to someone else.
    assert!(session_member_handles(leader, || true).is_empty());
    let _ = child.kill();
    let _ = child.wait();
    // Clean up the background sleep, which outlives the leader.
    for member in session_member_handles(leader, || false) {
        member.signal(Signal::Kill);
    }
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

// None of these clipboard tests touch the process environment: the command
// lists take the session as an argument and fake clipboard programs are run
// by absolute path with their output paths baked into the script. Test
// threads run concurrently, and a PATH or DISPLAY mutated here would leak
// into every other test that spawns a program.

fn clipboard_deadline() -> Instant {
    Instant::now() + CLIPBOARD_HELPER_TIMEOUT
}

fn fake_clipboard_dir(name: &str) -> shepr_test_support::ScratchDir {
    shepr_test_support::ScratchDir::new(name)
}

/// Write an executable script and return its absolute path as the
/// `'static` program name `ClipboardCommand` wants (leaked; tests only).
fn fake_clipboard_program(dir: &Path, name: &str, script: &str) -> &'static str {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.join(name);
    std::fs::write(&path, script).expect("fake clipboard program should be written");
    let mut permissions = std::fs::metadata(&path)
        .expect("fake clipboard program metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&path, permissions)
        .expect("fake clipboard program should be executable");
    Box::leak(path.to_string_lossy().into_owned().into_boxed_str())
}

fn quoted_path(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[test]
fn clipboard_commands_prefer_wayland_when_available() {
    let commands = clipboard_commands(ClipboardSession {
        wayland: true,
        x11: false,
    });
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].program, "wl-copy");
}

#[test]
fn clipboard_commands_are_empty_without_a_display_server() {
    let session = ClipboardSession {
        wayland: false,
        x11: false,
    };
    assert!(clipboard_commands(session).is_empty());
    assert!(read_clipboard_text_commands(session).is_empty());
}

#[test]
fn clipboard_program_name_strips_the_directory() {
    assert_eq!(clipboard_program_name("wl-copy"), "wl-copy");
    assert_eq!(clipboard_program_name("/usr/bin/wl-copy"), "wl-copy");
    assert_eq!(
        clipboard_program_name("/opt/wl-copy-wrapper"),
        "wl-copy-wrapper"
    );
}

#[test]
fn wl_copy_owner_does_not_block_clipboard_write() {
    use std::sync::mpsc;

    struct Cleanup {
        owner_pid: Option<i32>,
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(pid) = self.owner_pid {
                // SAFETY: kill(2) with a pid the fake wl-copy reported for
                // itself; it touches no memory of this process.
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
        }
    }

    // Declared before `cleanup`, so it is dropped (and the directory removed)
    // after the fake owner has been signalled.
    let temp_dir = fake_clipboard_dir("wl-copy");
    let mut cleanup = Cleanup { owner_pid: None };
    let marker = temp_dir.join("owner-pid");
    let payload = temp_dir.join("payload");
    let args = temp_dir.join("args");
    let fake_wl_copy = fake_clipboard_program(
        &temp_dir,
        "wl-copy",
        &format!(
            "#!/bin/sh\ncat > {payload}\nprintf '%s\\n' \"$@\" > {args}\nprintf '%s' \"$$\" > {marker}\nexec sleep 30\n",
            payload = quoted_path(&payload),
            args = quoted_path(&args),
            marker = quoted_path(&marker),
        ),
    );

    let (result_tx, result_rx) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        let command = ClipboardCommand {
            program: fake_wl_copy,
            args: &["--type", "text/plain;charset=utf-8"],
        };
        let _ = result_tx.send(run_clipboard_command(
            &command,
            b"clipboard text",
            clipboard_deadline(),
        ));
    });

    let marker_deadline = Instant::now() + Duration::from_secs(2);
    while !marker.exists() && Instant::now() < marker_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let owner_pid: i32 = std::fs::read_to_string(&marker)
        .expect("fake wl-copy should enter its clipboard-owner phase")
        .parse()
        .expect("owner pid should be numeric");
    cleanup.owner_pid = Some(owner_pid);
    let returned_while_owner_running = result_rx
        .recv_timeout(Duration::from_secs(2))
        .is_ok_and(|result| result);
    let actual_payload = std::fs::read(&payload).expect("fake wl-copy should record stdin");
    let actual_args = std::fs::read_to_string(&args).expect("fake wl-copy should record args");

    // SAFETY: kill(2) with the pid the fake wl-copy reported for itself.
    unsafe {
        libc::kill(owner_pid, libc::SIGTERM);
    }
    let reap_deadline = Instant::now() + Duration::from_secs(2);
    let owner_pid_u32 = u32::try_from(owner_pid).expect("owner pid should be positive");
    while process_exists(owner_pid_u32) && Instant::now() < reap_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let owner_was_reaped = !process_exists(owner_pid_u32);
    cleanup.owner_pid = None;
    writer.join().expect("clipboard writer thread should join");
    drop(cleanup);

    assert!(
        returned_while_owner_running,
        "clipboard writes must return while wl-copy remains alive to own the selection"
    );
    assert_eq!(actual_payload, b"clipboard text");
    assert_eq!(actual_args, "--type\ntext/plain;charset=utf-8\n");
    assert!(
        owner_was_reaped,
        "wl-copy owner should be reaped after exit"
    );
}

#[test]
fn failed_wl_copy_uses_x11_fallback() {
    let temp_dir = fake_clipboard_dir("fallback");
    let payload = temp_dir.join("xclip-payload");
    let fake_wl_copy = fake_clipboard_program(
        &temp_dir,
        "wl-copy",
        "#!/bin/sh\n/bin/cat >/dev/null\nexit 7\n",
    );
    let fake_xclip = fake_clipboard_program(
        &temp_dir,
        "xclip",
        &format!("#!/bin/sh\n/bin/cat > {}\n", quoted_path(&payload)),
    );

    // Same order `clipboard_commands` produces for a session with both a
    // Wayland and an X11 display, with the fakes standing in by path.
    let mut commands = clipboard_commands(ClipboardSession {
        wayland: true,
        x11: true,
    });
    assert_eq!(commands[0].program, "wl-copy");
    assert_eq!(commands[1].program, "xclip");
    commands[0].program = fake_wl_copy;
    commands[1].program = fake_xclip;

    let wrote = write_clipboard_with(&commands, b"clipboard fallback");
    let recorded = std::fs::read(&payload);

    assert!(wrote);
    assert_eq!(
        recorded.expect("xclip should record stdin"),
        b"clipboard fallback"
    );
}

#[test]
fn finite_clipboard_commands_report_exit_status() {
    let success = ClipboardCommand {
        program: "sh",
        args: &["-c", "cat >/dev/null"],
    };
    let failure = ClipboardCommand {
        program: "sh",
        args: &["-c", "cat >/dev/null; exit 7"],
    };

    assert!(run_clipboard_command(
        &success,
        b"clipboard text",
        clipboard_deadline()
    ));
    assert!(!run_clipboard_command(
        &failure,
        b"clipboard text",
        clipboard_deadline()
    ));
}

#[test]
fn a_clipboard_writer_that_hangs_is_killed_at_the_deadline() {
    // One helper never reads its input, the other reads it and never exits.
    let never_reads = ClipboardCommand {
        program: "sh",
        args: &["-c", "exec sleep 30"],
    };
    let never_exits = ClipboardCommand {
        program: "sh",
        args: &["-c", "cat >/dev/null; exec sleep 30"],
    };
    let payload = vec![b'x'; 1024 * 1024];
    for (command, bytes) in [(&never_reads, &payload[..]), (&never_exits, &b"text"[..])] {
        let started = Instant::now();
        let deadline = started + Duration::from_millis(200);
        assert!(!run_clipboard_command(command, bytes, deadline));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{command:?} held the write for {:?}",
            started.elapsed()
        );
    }
}

#[test]
fn clipboard_commands_include_x11_fallbacks() {
    let commands = clipboard_commands(ClipboardSession {
        wayland: false,
        x11: true,
    });
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].program, "xclip");
    assert_eq!(commands[1].program, "xsel");
}

#[test]
fn read_clipboard_text_commands_include_session_backends() {
    let commands = read_clipboard_text_commands(ClipboardSession {
        wayland: true,
        x11: true,
    });
    assert_eq!(commands[0].program, "wl-paste");
    assert_eq!(commands[1].program, "wl-paste");
    assert_eq!(commands[2].program, "xclip");
    assert_eq!(commands[3].program, "xsel");
}

#[test]
fn read_clipboard_text_with_command_reads_utf8() {
    let command = ClipboardCommand {
        program: "printf",
        args: &["feature/linear-302"],
    };

    assert_eq!(
        read_clipboard_text_with_command(&command, clipboard_deadline()).as_deref(),
        Some("feature/linear-302")
    );
}

#[test]
fn read_clipboard_text_with_command_rejects_oversized_output() {
    let command = ClipboardCommand {
        program: "sh",
        args: &["-c", "yes x | head -c 1048578"],
    };

    assert_eq!(
        read_clipboard_text_with_command(&command, clipboard_deadline()),
        None
    );
}

#[test]
fn a_clipboard_reader_that_hangs_is_killed_at_the_deadline() {
    // Silent, half an answer with the pipe left open, and a full answer from
    // a helper that then never exits.
    let silent = ClipboardCommand {
        program: "sh",
        args: &["-c", "exec sleep 30"],
    };
    let partial = ClipboardCommand {
        program: "sh",
        args: &["-c", "printf partial; exec sleep 30"],
    };
    let lingering = ClipboardCommand {
        program: "sh",
        args: &["-c", "printf text; exec >&-; exec sleep 30"],
    };
    for command in [&silent, &partial, &lingering] {
        let started = Instant::now();
        let deadline = started + Duration::from_millis(200);
        assert_eq!(read_clipboard_text_with_command(command, deadline), None);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{command:?} held the read for {:?}",
            started.elapsed()
        );
    }
}
