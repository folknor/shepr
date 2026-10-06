use super::*;
use shepr_test_support::fixture::{self, Held, Step};
use std::{
    io::Read,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

// ---------------------------------------------------------------------------
// Child exit, raw reads, terminal
// ---------------------------------------------------------------------------

#[test]
fn child_exit_classification_separates_codes_from_signals() {
    use std::os::unix::process::ExitStatusExt;

    for code in [0, 1, 130, 255] {
        // Raw wait status: exit code in bits 8..16, no terminating signal.
        let kind = classify_child_exit(&std::process::ExitStatus::from_raw(code << 8));
        assert_eq!(kind, ChildExitKind::Exited, "exit code {code:#x}");
    }
    let status = std::process::ExitStatus::from_raw(libc::SIGTERM);
    assert_eq!(classify_child_exit(&status), ChildExitKind::Signalled);
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
fn proc_stat_yields_session_and_controlling_tty() {
    assert_eq!(
        session_and_tty_from_stat(
            "4242 (shepr (srv) x) S 1 4242 4242 0 -1 4194560 0 0 0 0 0 0 0 0 0 0 0 0 0"
        ),
        Some((4242, 0))
    );
    assert_eq!(
        session_and_tty_from_stat(
            "77 (shepr) S 70 77 77 34817 77 4194560 0 0 0 0 0 0 0 0 0 0 0 0 0"
        ),
        Some((77, 34817))
    );
    assert_eq!(session_and_tty_from_stat("77 (shepr) S 70"), None);
}

#[test]
fn launch_executable_follows_a_replaced_binary_to_its_new_install() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    fn with_proc_deleted_suffix(path: &Path) -> PathBuf {
        let mut bytes = path.as_os_str().as_bytes().to_vec();
        bytes.extend_from_slice(crate::proc_tree::PROC_DELETED_SUFFIX);
        PathBuf::from(std::ffi::OsString::from_vec(bytes))
    }

    let installed = |path: &Path| Ok(path == Path::new("/usr/bin/shepr"));
    // A running binary that an install replaced.
    assert_eq!(
        resolve_launch_executable(
            with_proc_deleted_suffix(Path::new("/usr/bin/shepr")),
            installed,
        )
        .expect("test precondition"),
        PathBuf::from("/usr/bin/shepr")
    );
    // The normal case: the path is there, nothing is rewritten.
    assert_eq!(
        resolve_launch_executable(PathBuf::from("/usr/bin/shepr"), installed)
            .expect("test precondition"),
        PathBuf::from("/usr/bin/shepr")
    );
    // Removed with no replacement: keep the reported path, there is nothing
    // better to offer.
    let missing = with_proc_deleted_suffix(Path::new("/opt/shepr"));
    assert_eq!(
        resolve_launch_executable(missing.clone(), installed).expect("test precondition"),
        missing
    );
    // A binary whose real name ends in the suffix is left alone.
    let literal_path = with_proc_deleted_suffix(Path::new("/opt/shepr"));
    let literal = |path: &Path| Ok(path == literal_path.as_path());
    assert_eq!(
        resolve_launch_executable(literal_path.clone(), literal).expect("test precondition"),
        literal_path
    );
    // A stat failure other than absence is reported, not read as absence.
    let denied = |_: &Path| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    assert!(resolve_launch_executable(PathBuf::from("/usr/bin/shepr"), denied).is_err());
}

// ---------------------------------------------------------------------------
// Owned runtime directories
// ---------------------------------------------------------------------------

/// A regular-file family, standing in for a caller's own.
const TEST_FILES: DirectoryKind = DirectoryKind::regular_file("shepr-test-", "config");

#[test]
fn owned_directory_is_private_and_under_the_parent() {
    use std::os::unix::fs::PermissionsExt;

    let runtime_dir = shepr_test_support::ScratchDir::new("owned-directory-runtime");
    let first = create_owned_directory(runtime_dir.path(), TEST_FILES).expect("test precondition");
    let second = create_owned_directory(runtime_dir.path(), TEST_FILES).expect("test precondition");
    assert!(first.starts_with(runtime_dir.path()));
    assert_ne!(first, second);
    assert_eq!(
        std::fs::metadata(&first)
            .expect("test precondition")
            .permissions()
            .mode()
            & crate::limits::PERMISSION_BITS,
        crate::limits::PRIVATE_DIRECTORY_MODE
    );
}

#[test]
fn private_directory_requirement_rejects_symlinks_and_public_modes() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let runtime_dir = shepr_test_support::ScratchDir::new("private-directory-validation");
    let dir = create_owned_directory(runtime_dir.path(), TEST_FILES).expect("test precondition");
    let link = dir.join("link");
    symlink(&dir, &link).expect("test precondition");
    assert!(matches!(
        require_private_directory(&link),
        Err(PrivateDirError::Policy)
    ));
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
        .expect("test precondition");
    assert!(matches!(
        require_private_directory(&dir),
        Err(PrivateDirError::Policy)
    ));
}

#[test]
fn an_owned_runtime_directory_found_open_is_tightened() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let scratch = shepr_test_support::ScratchDir::new("private-runtime-directory");
    let runtime = scratch.join("shepr");
    std::fs::create_dir(&runtime).expect("test precondition");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755))
        .expect("test precondition");

    crate::create_private_runtime_directory(&runtime).expect("an owned directory is tightened");
    assert!(require_private_directory(&runtime).is_ok());

    // A missing directory is created private.
    let fresh = scratch.join("fresh/shepr");
    crate::create_private_runtime_directory(&fresh).expect("a missing directory is created");
    assert!(require_private_directory(&fresh).is_ok());

    // A symlink is neither followed nor changed; the caller's check refuses it.
    let link = scratch.join("link");
    symlink(&runtime, &link).expect("test precondition");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755))
        .expect("test precondition");
    crate::create_private_runtime_directory(&link).expect("a symlink is left alone");
    assert!(matches!(
        require_private_directory(&link),
        Err(PrivateDirError::Policy)
    ));
    assert_eq!(
        std::fs::metadata(&runtime)
            .expect("stat the target")
            .permissions()
            .mode()
            & crate::limits::PERMISSION_BITS,
        0o755,
        "the symlink's target is not touched"
    );
}

#[test]
fn startup_sweeps_only_owned_paths_with_a_proven_dead_process() {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let runtime = shepr_test_support::ScratchDir::new("platform-stale-sweep");
    std::fs::set_permissions(
        runtime.path(),
        std::fs::Permissions::from_mode(crate::limits::PRIVATE_DIRECTORY_MODE),
    )
    .expect("test precondition");
    let dead_tag = dead_process_tag();
    let live_tag = process_identity::ProcessIdentity::current()
        .expect("current process identity")
        .tag();

    let ssh_config_kind = TEST_FILES;
    let staging_kind = crate::owned_runtime::DirectoryKind::STAGING;
    let stale_config = runtime.join(ssh_config_kind.directory_name(1));
    std::fs::create_dir(&stale_config).expect("test precondition");
    std::fs::set_permissions(
        &stale_config,
        std::fs::Permissions::from_mode(crate::limits::PRIVATE_DIRECTORY_MODE),
    )
    .expect("test precondition");
    std::fs::write(ssh_config_kind.content_path(&stale_config), b"Host *\n")
        .expect("test precondition");
    let untagged_config = runtime.join(ssh_config_kind.directory_name(0x0123_4567_89ab_cdef));
    std::fs::create_dir(&untagged_config).expect("test precondition");
    let live_config = runtime.join(ssh_config_kind.directory_name(2));
    std::fs::create_dir(&live_config).expect("test precondition");

    for (kind, token, tag) in [
        (ssh_config_kind, 1, dead_tag.as_str()),
        (ssh_config_kind, 2, live_tag.as_str()),
        (staging_kind, 1, dead_tag.as_str()),
        (staging_kind, 2, live_tag.as_str()),
    ] {
        let staging = runtime.join(kind.directory_name(token));
        if !staging.try_exists().expect("stat test fixture") {
            std::fs::create_dir(&staging).expect("test precondition");
        }
        std::fs::set_permissions(
            &staging,
            std::fs::Permissions::from_mode(crate::limits::PRIVATE_DIRECTORY_MODE),
        )
        .expect("test precondition");
        let mut marker = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(crate::limits::RUNTIME_MARKER_MODE)
            .open(staging.join(".owner"))
            .expect("test precondition");
        marker.write_all(tag.as_bytes()).expect("test precondition");
    }

    let _created = create_owned_directory(runtime.path(), TEST_FILES).expect("create config dir");
    assert!(
        !present(&stale_config),
        "dead owner's config directory is swept"
    );
    assert!(
        present(&untagged_config),
        "an untagged directory has no provable owner"
    );
    assert!(
        present(&live_config),
        "a live owner's config directory is retained"
    );

    let socket_path = runtime.join("api.sock");
    let listener = ipc::bind_private_local_listener(&socket_path).expect("bind listener");
    assert!(
        !present(&runtime.join(staging_kind.directory_name(1))),
        "dead owner's staging directory is swept"
    );
    assert!(
        present(&runtime.join(staging_kind.directory_name(2))),
        "a live owner's staging directory is retained"
    );
    drop(listener);
}

fn present(path: &Path) -> bool {
    path.try_exists().expect("stat a swept path")
}

fn dead_process_tag() -> String {
    let current = process_identity::ProcessIdentity::current().expect("current process identity");
    let tag = current.tag();
    let (_, rest) = tag.split_once('-').expect("serialized process identity");
    format!("{:08x}-{rest}", i32::MAX)
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
fn config_metadata_preserves_acl_without_inheriting_extra_access() {
    use std::os::unix::fs::MetadataExt;

    // Scratch lives on the build tree's filesystem, so this probe exercises
    // POSIX ACL xattrs there rather than on whatever the host temp mount is.
    let dir = shepr_test_support::ScratchDir::new("config-acl");
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
        if has_acl {
            set_attribute(&input, c"system.posix_acl_access", &acl);
        }
        set_attribute(&input, c"user.shepr-test", b"preserve this attribute");
        let original = input.metadata().expect("test precondition");
        let staged = create_private_file(&target).expect("test precondition");
        let output = std::fs::File::open(&target).expect("test precondition");
        // Model a default ACL inherited from the destination's parent directory.
        set_attribute(&output, c"system.posix_acl_access", &acl);
        write_config_temporary(Some(&source), staged, b"new").expect("test precondition");
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

#[test]
#[ignore = "requires root with mapped uid/gid 1001:1002 for fchown"]
fn config_metadata_preserves_a_different_source_owner() {
    use std::os::unix::fs::MetadataExt;

    // A non-root process cannot create a differently owned source here. The
    // tolerated EPERM path has a separate unit test through the ownership seam.
    assert_eq!(effective_uid(), 0, "run this test as root");
    let dir = shepr_test_support::ScratchDir::new("config-owner");
    let source = dir.join("source");
    let target = dir.join("target");
    std::fs::write(&source, b"old").expect("test precondition");
    let input = std::fs::File::open(&source).expect("test precondition");
    // SAFETY: fchown(2) on an fd `input` keeps open; integers only.
    assert_eq!(unsafe { libc::fchown(input.as_raw_fd(), 1001, 1002) }, 0);
    let original = input.metadata().expect("test precondition");
    let staged = create_private_file(&target).expect("test precondition");

    write_config_temporary(Some(&source), staged, b"new").expect("test precondition");

    let actual = std::fs::metadata(&target).expect("test precondition");
    assert_eq!(
        (actual.uid(), actual.gid()),
        (original.uid(), original.gid())
    );
    assert_eq!(std::fs::read(&target).expect("test precondition"), b"new");
}

// ---------------------------------------------------------------------------
// Process handles and session teardown
// ---------------------------------------------------------------------------

#[test]
fn process_handle_follows_one_process_through_exit_and_reap() {
    let mut child = fixture::command(&[Step::Sleep(Duration::from_secs(30))])
        .spawn()
        .expect("spawn sleep");
    let handle = ProcessHandle::open(Pid::new(child.id()).expect("child pid"))
        .expect("pidfd_open on a live child");
    assert_eq!(handle.process_id().get(), child.id());
    assert!(handle.is_unreaped());
    assert!(!handle.has_exited());

    assert!(handle.signal(Signal::Kill));
    assert!(wait_for_process_exits(&[&handle], Duration::from_secs(5)));
    // A zombie has exited but still holds its pid.
    assert!(handle.has_exited());
    assert!(handle.is_unreaped());

    child.wait().expect("reap sleep");
    assert!(!handle.is_unreaped());
    assert!(
        !handle.signal(Signal::Kill),
        "a reaped process's handle must not signal anything"
    );
}

#[test]
fn wait_for_process_exits_times_out_on_a_live_process() {
    let mut child = fixture::command(&[Step::Sleep(Duration::from_secs(30))])
        .spawn()
        .expect("spawn sleep");
    let handle = ProcessHandle::open(Pid::new(child.id()).expect("child pid"))
        .expect("pidfd_open on a live child");
    assert!(!wait_for_process_exits(
        &[&handle],
        Duration::from_millis(30)
    ));
    child.kill().expect("kill sleep");
    child.wait().expect("reap sleep");
}

fn spawn_session_with_background_job() -> std::process::Child {
    use std::os::unix::process::CommandExt as _;

    let mut command = fixture::command(&[
        Step::Spawn {
            argv0: "background-job".into(),
            sleep: Duration::from_secs(30),
            held: Held::All,
        },
        Step::Sleep(Duration::from_secs(30)),
    ]);
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

// These tests observe when the kernel publishes real child membership in
// /proc. Their short sleeps yield to process startup; a fake clock cannot make
// that external state appear.
#[test]
fn session_members_are_found_without_the_leader_and_signalled_by_handle() {
    let mut child = spawn_session_with_background_job();
    let leader = child.id();
    let deadline = Instant::now() + Duration::from_secs(5);
    let members = loop {
        let members = session_members(
            SessionId::of_leader(Pid::new(leader).expect("session leader")),
            || false,
        );
        if !members.is_empty() || Instant::now() >= deadline {
            break members;
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    assert!(
        members
            .iter()
            .all(|member| member.process_id().get() != leader),
        "the leader is not a member handle"
    );
    assert_eq!(members.len(), 1, "the background sleep is the only member");
    for member in &members {
        assert!(member.signal(Signal::Kill));
    }
    child.kill().expect("kill session leader");
    child.wait().expect("reap session leader");
    let handles: Vec<&ProcessHandle> = members.iter().collect();
    assert!(wait_for_process_exits(&handles, Duration::from_secs(5)));
}

#[test]
fn session_members_are_withheld_when_a_reaped_leaders_pid_is_held_again() {
    let mut child = spawn_session_with_background_job();
    let leader = child.id();
    let deadline = Instant::now() + Duration::from_secs(5);
    let members = loop {
        let members = session_members(
            SessionId::of_leader(Pid::new(leader).expect("session leader")),
            || false,
        );
        if !members.is_empty() || Instant::now() >= deadline {
            break members;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(members.len(), 1, "background sleep must be running");
    // The leader is alive, so from the point of view of a caller that has
    // already reaped its own leader, pid `leader` belongs to someone else.
    assert!(
        session_members(
            SessionId::of_leader(Pid::new(leader).expect("session leader")),
            || true,
        )
        .is_empty()
    );
    child.kill().expect("kill session leader");
    child.wait().expect("reap session leader");
    // Clean up the background sleep, which outlives the leader.
    for member in &members {
        assert!(
            member.signal(Signal::Kill),
            "kill background session member"
        );
    }
    let handles: Vec<&ProcessHandle> = members.iter().collect();
    assert!(wait_for_process_exits(&handles, Duration::from_secs(5)));
}

#[test]
fn server_daemon_detach_creates_new_session() {
    let mut command = fixture::command(&[Step::PrintSid]);
    detach_server_daemon_command(&mut command);
    let child = command
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("test precondition");
    let pid = child.id();
    let output = child.wait_with_output().expect("test precondition");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        pid.to_string(),
        "detached server child should be its own session leader"
    );
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

// None of these clipboard tests touch the process environment: the command
// lists take the session as an argument and fake clipboard programs are
// fixture stand-ins run by absolute path, with their output paths in their
// scripts. Test threads run concurrently, and a `PATH` or `DISPLAY` mutated
// here would leak into every other test that spawns a program.

fn clipboard_deadline() -> Instant {
    Instant::now() + CLIPBOARD_HELPER_TIMEOUT
}

fn fake_clipboard_dir(name: &str) -> shepr_test_support::ScratchDir {
    shepr_test_support::ScratchDir::new(name)
}

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

/// Install a fixture stand-in under the requested helper name and return its
/// absolute path as the `'static` program name `ClipboardCommand` wants
/// (leaked; tests only).
fn fake_clipboard_program(dir: &Path, name: &str, steps: &[Step]) -> &'static str {
    leak(
        fixture::stand_in(dir, name, steps)
            .to_string_lossy()
            .into_owned(),
    )
}

/// A clipboard command running the fixture with this script (leaked; tests
/// only).
fn fixture_clipboard_command(steps: &[Step]) -> ClipboardCommand {
    let args: Vec<&'static str> = fixture::args(steps)
        .into_iter()
        .map(|token| leak(token.into_string().expect("a UTF-8 fixture token")))
        .collect();
    ClipboardCommand {
        program: fixture::path_str(),
        args: Box::leak(args.into_boxed_slice()),
        owns_selection_after_exit: false,
    }
}

fn sleep_30() -> Step {
    Step::Sleep(Duration::from_secs(30))
}

#[test]
fn clipboard_commands_prefer_wayland_when_available() {
    let commands = clipboard_commands(ClipboardSession {
        wayland: true,
        x11: false,
    });
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].program, "wl-copy");
    assert!(commands[0].owns_selection_after_exit);
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
fn selection_owning_helper_does_not_block_clipboard_write() {
    use std::sync::mpsc;

    struct Cleanup {
        owner_pid: Option<i32>,
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(pid) = self.owner_pid {
                // SAFETY: kill(2) with a pid the fake selection helper reported for
                // itself; it touches no memory of this process.
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
            }
        }
    }

    let helper_dir = fake_clipboard_dir("wl-copy-wrapper");
    let mut cleanup = Cleanup { owner_pid: None };
    let marker = helper_dir.join("owner-pid");
    let payload = helper_dir.join("payload");
    let args = helper_dir.join("args");
    let fake_owner = fake_clipboard_program(
        &helper_dir,
        "wl-copy-wrapper",
        &[
            Step::To(payload.clone()),
            Step::Cat,
            Step::To(args.clone()),
            Step::PrintArgs,
            Step::To(marker.clone()),
            Step::PrintPid,
            sleep_30(),
        ],
    );

    let (result_tx, result_rx) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        let command = ClipboardCommand {
            program: fake_owner,
            args: &["--type", "text/plain;charset=utf-8"],
            owns_selection_after_exit: true,
        };
        result_tx
            .send(run_clipboard_command(
                &command,
                b"clipboard text",
                clipboard_deadline(),
            ))
            .expect("the test holds the receiver until the writer joins");
    });

    // This marker is written by a real helper process. Polling waits for the
    // helper's filesystem write; the deadline only guards a broken harness.
    let marker_deadline = Instant::now() + Duration::from_secs(2);
    let owner_pid: i32 = loop {
        match std::fs::read_to_string(&marker)
            .ok()
            .and_then(|pid| pid.parse().ok())
        {
            Some(pid) => break pid,
            None if Instant::now() < marker_deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            None => panic!("fake selection helper should enter its owner phase"),
        }
    };
    cleanup.owner_pid = Some(owner_pid);
    let returned_while_owner_running = result_rx
        .recv_timeout(Duration::from_secs(2))
        .is_ok_and(|result| result);
    let actual_payload = std::fs::read(&payload).expect("fake helper should record stdin");
    let actual_args = std::fs::read_to_string(&args).expect("fake helper should record args");

    // SAFETY: kill(2) with the pid the fake selection helper reported for itself.
    unsafe {
        libc::kill(owner_pid, libc::SIGTERM);
    }
    let reap_deadline = Instant::now() + Duration::from_secs(2);
    let owner_pid_u32 = u32::try_from(owner_pid).expect("owner pid should be positive");
    let owner_pid = Pid::new(owner_pid_u32).expect("owner pid");
    while process_exists(owner_pid) && Instant::now() < reap_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let owner_was_reaped = !process_exists(owner_pid);
    cleanup.owner_pid = None;
    writer.join().expect("clipboard writer thread should join");
    drop(cleanup);

    assert!(
        returned_while_owner_running,
        "clipboard writes must return while the helper remains alive to own the selection"
    );
    assert_eq!(actual_payload, b"clipboard text");
    assert_eq!(actual_args, "--type\ntext/plain;charset=utf-8\n");
    assert!(
        owner_was_reaped,
        "selection owner should be reaped after exit"
    );
}

#[test]
fn failed_wl_copy_uses_x11_fallback() {
    let temp_dir = fake_clipboard_dir("fallback");
    let payload = temp_dir.join("xclip-payload");
    let fake_wl_copy = fake_clipboard_program(&temp_dir, "wl-copy", &[Step::Drain, Step::Exit(7)]);
    let fake_xclip =
        fake_clipboard_program(&temp_dir, "xclip", &[Step::To(payload.clone()), Step::Cat]);

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

    let wrote = write_clipboard_and_primary_with(&commands, &[], b"clipboard fallback");
    let recorded = std::fs::read(&payload);

    assert!(wrote);
    assert_eq!(
        recorded.expect("xclip should record stdin"),
        b"clipboard fallback"
    );
}

#[test]
fn finite_clipboard_commands_report_exit_status() {
    let success = fixture_clipboard_command(&[Step::Drain]);
    let failure = fixture_clipboard_command(&[Step::Drain, Step::Exit(7)]);

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
fn no_clipboard_helper_is_started_once_the_deadline_has_passed() {
    let temp_dir = fake_clipboard_dir("late-helper");
    let payload = temp_dir.join("payload");
    let helper = ClipboardCommand {
        program: fake_clipboard_program(
            &temp_dir,
            "xclip",
            &[Step::To(payload.clone()), Step::Cat],
        ),
        args: &[],
        owns_selection_after_exit: false,
    };

    // The real clock reads the deadline itself or later.
    assert!(!run_clipboard_command(
        &helper,
        b"clipboard text",
        Instant::now()
    ));
    assert!(
        !payload.try_exists().expect("stat the payload path"),
        "a helper started after the deadline ran"
    );
}

#[test]
fn a_clipboard_writer_that_hangs_is_killed_at_the_deadline() {
    // One helper never reads its input, the other reads it and never exits.
    let never_reads = fixture_clipboard_command(&[sleep_30()]);
    let never_exits = fixture_clipboard_command(&[Step::Drain, sleep_30()]);
    let payload = vec![b'x'; 1024 * 1024];
    for (command, bytes) in [(&never_reads, &payload[..]), (&never_exits, &b"text"[..])] {
        let started = Instant::now();
        let (now, deadline) = stepping_clipboard_clock(started);
        assert!(!run_clipboard_command_with_clock(
            command, bytes, deadline, &now
        ));
        // Only a broken deadline check can wait for the 30-second helper.
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
    let command = fixture_clipboard_command(&[Step::Print("feature/linear-302".into())]);

    assert_eq!(
        read_clipboard_text_with_command(&command, clipboard_deadline()).as_deref(),
        Some("feature/linear-302")
    );
}

#[test]
fn read_clipboard_text_with_command_rejects_oversized_output() {
    // Two bytes past the one-mebibyte cap.
    let command = fixture_clipboard_command(&[Step::Fill {
        byte: b'x',
        count: crate::limits::MAX_CLIPBOARD_TEXT_BYTES + 2,
    }]);

    assert_eq!(
        read_clipboard_text_with_command(&command, clipboard_deadline()),
        None
    );
}

#[test]
fn a_clipboard_reader_that_hangs_is_killed_at_the_deadline() {
    // Silent, half an answer with the pipe left open, and a full answer from
    // a helper that then never exits.
    let silent = fixture_clipboard_command(&[sleep_30()]);
    let partial = fixture_clipboard_command(&[Step::Print("partial".into()), sleep_30()]);
    let lingering =
        fixture_clipboard_command(&[Step::Print("text".into()), Step::CloseStdout, sleep_30()]);
    for command in [&silent, &partial, &lingering] {
        let started = Instant::now();
        let (now, deadline) = stepping_clipboard_clock(started);
        assert_eq!(
            read_clipboard_text_with_command_with_clock(command, deadline, &now),
            None
        );
        // The injected clock reaches the deadline after a few reads, so only a
        // broken deadline check can wait for the 30-second helper.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{command:?} held the read for {:?}",
            started.elapsed()
        );
    }
}

/// A clock that moves a quarter of the way to its deadline on every read, so
/// a helper wait passes the deadline after a fixed number of clock reads
/// whatever the machine's speed, while each real poll stays short.
fn stepping_clipboard_clock(
    start: Instant,
) -> (std::sync::Arc<dyn Fn() -> Instant + Send + Sync>, Instant) {
    let step = Duration::from_millis(50);
    let deadline = start + step * 4;
    let reads = std::sync::atomic::AtomicU32::new(0);
    let now: std::sync::Arc<dyn Fn() -> Instant + Send + Sync> = std::sync::Arc::new(move || {
        start + step * reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    });
    (now, deadline)
}
