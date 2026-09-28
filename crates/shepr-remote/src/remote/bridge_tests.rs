use super::*;
use interprocess::local_socket::traits::Stream as _;
use shepr_core::socket_path::fits_unix_socket_path;
use std::fs;

fn upload_test_streams(
    name: &str,
) -> (
    shepr_platform::ipc::LocalStream,
    shepr_platform::ipc::LocalStream,
) {
    let scratch = shepr_test_support::ScratchDir::new(name);
    let socket = scratch.join("upload.sock");
    let listener =
        shepr_platform::ipc::bind_private_local_listener(&socket).expect("test precondition");
    let client = shepr_platform::ipc::connect_local_stream(&socket).expect("test precondition");
    let server = listener.accept().expect("test precondition");
    server.set_nonblocking(true).expect("test precondition");
    drop(listener);
    std::fs::remove_file(socket).expect("test precondition");
    (client, server)
}

#[test]
fn bridge_upload_idle_waits_without_repeated_reads_and_cancels() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;

    let (mut client, stream) = upload_test_streams("idle");
    let attempts = Arc::new(AtomicUsize::new(0));
    let worker_attempts = Arc::clone(&attempts);
    let stop = Arc::new(BridgeUploadStop::new().expect("test precondition"));
    let worker_stop = Arc::clone(&stop);
    let (done_tx, done_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        super::super::bridge::UPLOAD_READ_ATTEMPTS
            .with(|slot| *slot.borrow_mut() = Some(worker_attempts));
        let mut output = Vec::new();
        let closed = AtomicBool::new(false);
        let result = copy_local_stream_to_writer(
            stream,
            &mut output,
            &worker_stop,
            &AtomicBool::new(false),
            &closed,
        );
        done_tx
            .send((result, output, closed.load(Ordering::Acquire)))
            .expect("test precondition");
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while attempts.load(Ordering::Relaxed) == 0 {
        assert!(Instant::now() < deadline, "upload worker did not start");
        thread::sleep(Duration::from_millis(1));
    }
    thread::sleep(Duration::from_millis(100));
    let idle_reads = attempts.load(Ordering::Relaxed);
    client.write_all(b"pane input").expect("test precondition");
    let deadline = Instant::now() + Duration::from_secs(5);
    while attempts.load(Ordering::Relaxed) < idle_reads + 2 {
        assert!(
            Instant::now() < deadline,
            "input did not wake the upload worker"
        );
        thread::sleep(Duration::from_millis(1));
    }
    thread::sleep(Duration::from_millis(100));
    let reads_after_input = attempts.load(Ordering::Relaxed);
    stop.cancel();
    let (result, output, closed) = done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("test precondition");
    worker.join().expect("test precondition");
    assert_eq!(result.expect("test precondition"), 10);
    assert_eq!(output, b"pane input");
    assert!(!closed, "cancellation is not a peer disconnect");
    assert_eq!(idle_reads, 1, "idle forwarding must wait, not retry reads");
    assert_eq!(
        reads_after_input, 3,
        "forwarding must sleep again after input"
    );
}

#[test]
fn bridge_upload_cancel_before_wait_preserves_download() {
    use std::io::Read as _;

    let (mut client, stream) = upload_test_streams("cancel-before-wait");
    let mut download = stream.try_clone().expect("test precondition");
    let stop = BridgeUploadStop::new().expect("test precondition");
    stop.cancel();
    stop.cancel();
    let closed = AtomicBool::new(false);
    let count = copy_local_stream_to_writer(
        stream,
        &mut Vec::new(),
        &stop,
        &AtomicBool::new(false),
        &closed,
    )
    .expect("test precondition");
    assert_eq!(count, 0);
    assert!(!closed.load(Ordering::Acquire));
    download
        .write_all(b"final frame")
        .expect("test precondition");
    let mut output = [0; 11];
    client.read_exact(&mut output).expect("test precondition");
    assert_eq!(&output, b"final frame");
}

#[test]
fn bridge_upload_cancel_between_stop_check_and_wait_is_retained() {
    let (_client, stream) = upload_test_streams("cancel-before-poll");
    let stop = BridgeUploadStop::new().expect("test precondition");
    assert!(!stop.is_stopped());
    stop.cancel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        done_tx
            .send(stop.wake.wait(&stream))
            .expect("test precondition");
    });
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("test precondition")
        .expect("test precondition");
    worker.join().expect("test precondition");
}

#[test]
fn bridge_upload_drains_input_before_peer_eof() {
    let (mut client, stream) = upload_test_streams("drain");
    let payload = vec![b'x'; 1024 * 1024];
    let expected = payload.clone();
    let worker = thread::spawn(move || {
        let stop = BridgeUploadStop::new().expect("test precondition");
        let mut output = Vec::new();
        let closed = AtomicBool::new(false);
        let count = copy_local_stream_to_writer(
            stream,
            &mut output,
            &stop,
            &AtomicBool::new(false),
            &closed,
        )
        .expect("test precondition");
        assert!(closed.load(Ordering::Acquire));
        assert_eq!(count, output.len() as u64);
        output
    });
    client.write_all(&payload).expect("test precondition");
    drop(client);
    assert_eq!(worker.join().expect("test precondition"), expected);
}

#[test]
fn bridge_socket_is_user_only() {
    use std::os::unix::fs::PermissionsExt;

    let scratch = shepr_test_support::ScratchDir::new("bridge-mode");
    let socket = scratch.join("bridge.sock");
    let remote_shepr = RemoteExecutable::parse("/usr/bin/shepr").expect("test precondition");
    let bridge = SshStdioBridge::start(
        SshTarget::parse("example").expect("test precondition"),
        &remote_shepr,
        socket.clone(),
        "default",
        None,
        false,
    )
    .expect("start bridge listener");

    let mode = std::fs::metadata(&socket)
        .expect("test precondition")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);

    drop(bridge);
    // Dropping the bridge removes the socket it owns.
    assert_eq!(
        std::fs::symlink_metadata(&socket)
            .expect_err("dropped bridge left its socket behind")
            .kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn accepted_bridge_stream_is_reset_to_blocking() {
    use std::os::fd::AsRawFd as _;

    fn is_nonblocking(stream: &shepr_platform::ipc::LocalStream) -> bool {
        let fd = match stream {
            shepr_platform::ipc::LocalStream::UdSocket(stream) => stream.inner().as_raw_fd(),
        };
        // SAFETY: F_GETFL only reads flags from the live descriptor owned by `stream`.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(flags >= 0, "fcntl(F_GETFL): {}", io::Error::last_os_error());
        flags & libc::O_NONBLOCK != 0
    }

    let scratch = shepr_test_support::ScratchDir::new("bridge-blocking");
    let socket = scratch.join("bridge.sock");
    let listener =
        shepr_platform::ipc::bind_private_local_listener(&socket).expect("bind listener");
    let client = shepr_platform::ipc::connect_local_stream(&socket).expect("connect client");
    let mut server = listener.accept().expect("accept client");

    shepr_platform::ipc::set_local_stream_polling(&mut server, true)
        .expect("force a nonblocking accepted stream");
    assert!(is_nonblocking(&server));
    let server = prepare_remote_bridge_stream(server).expect("prepare bridge stream");
    assert!(!is_nonblocking(&server));

    // The socket file stays in the test's own scratch directory, which the
    // next hand-out clears.
    drop(server);
    drop(client);
    drop(listener);
}

#[test]
fn bridge_drop_while_waiting_for_client_is_bounded() {
    let runtime_dir = shepr_test_support::ScratchDir::new("bb");
    let socket = local_forward_socket_path(runtime_dir.path(), "drop-test", "default")
        .expect("test precondition");
    assert!(
        socket.starts_with(runtime_dir.path()),
        "{}",
        socket.display()
    );
    let remote_shepr = RemoteExecutable::parse("/usr/bin/shepr").expect("test precondition");
    let bridge = SshStdioBridge::start(
        SshTarget::parse("example").expect("test precondition"),
        &remote_shepr,
        socket.clone(),
        "default",
        None,
        false,
    )
    .expect("start bridge listener");
    let started = Instant::now();

    drop(bridge);

    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(!socket.try_exists().expect("stat bridge socket"));
}

fn exit_status(code: i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt as _;
    std::process::ExitStatus::from_raw(code << 8)
}

#[test]
fn only_ssh_own_exit_code_counts_as_a_link_failure() {
    let link = ssh_bridge_exit_error(
        exit_status(SSH_OWN_FAILURE_EXIT_CODE),
        b"Connection refused",
    );
    assert!(is_ssh_link_failure(&link));
    assert_eq!(link.kind(), io::ErrorKind::ConnectionAborted);
    assert_eq!(
        link.to_string(),
        format!(
            "remote SSH connection failed (exit status {SSH_OWN_FAILURE_EXIT_CODE}): Connection refused"
        )
    );
    let remapped = ssh_bridge_exit_error(
        exit_status(REMAPPED_REMOTE_255_EXIT_CODE),
        b"remote bridge failed",
    );
    assert!(!is_ssh_link_failure(&remapped));
    let remapped_message = remapped.to_string();
    assert!(remapped_message.contains(&format!(
        "remote status {SSH_OWN_FAILURE_EXIT_CODE} is remapped to {REMAPPED_REMOTE_255_EXIT_CODE}"
    )));
    assert!(remapped_message.contains(&format!(
        "a native {REMAPPED_REMOTE_255_EXIT_CODE} is indistinguishable"
    )));
    let missing = ssh_bridge_exit_error(exit_status(127), b"sh: 1: exec: /old/shepr: not found");
    assert!(!is_ssh_link_failure(&missing));
    assert_eq!(
        missing.to_string(),
        "remote command failed (exit status 127): sh: 1: exec: /old/shepr: not found"
    );
    let stale = ssh_bridge_exit_error(exit_status(78), super::STALE_API_METADATA.as_bytes());
    assert!(!is_ssh_link_failure(&stale));
    assert!(crate::SavedSshApiBridge::stale_metadata_failure(&stale));
    assert!(is_ssh_link_failure(&io::Error::new(
        io::ErrorKind::TimedOut,
        "handshake timed out"
    )));
    assert!(!is_ssh_link_failure(&io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "closed before welcome"
    )));
}

#[test]
fn bridge_remote_stderr_is_filtered_before_error_output() {
    let error = ssh_bridge_exit_error(
        exit_status(SSH_OWN_FAILURE_EXIT_CODE),
        b"Connection refused\x1b[2J",
    );
    assert!(!error.to_string().contains('\x1b'));
    assert!(error.to_string().contains("Connection refused?[2J"));
}

#[test]
fn sanitize_path_component_removes_shell_sensitive_chars() {
    assert_eq!(sanitize_path_component("user@host:22"), "user-host-22");
}

#[test]
fn remote_output_framing_discards_any_banner_and_preserves_binary() {
    let payload = [0, 1, 2, 0xff, b'\n'];
    let mut input = vec![b'x'; 4 * 1024 * 1024];
    input.extend_from_slice(b"\r\nshepr-remote-output-ready\r\n");
    input.extend_from_slice(&payload);
    let mut reader = io::BufReader::with_capacity(17, io::Cursor::new(input));

    discard_remote_output_preamble(&mut reader).expect("test precondition");
    let mut output = Vec::new();
    io::Read::read_to_end(&mut reader, &mut output).expect("test precondition");
    assert_eq!(output, payload);

    let mut missing = b"profile output without marker".to_vec();
    assert!(normalize_remote_stdout(&mut missing, true).is_err());
    normalize_remote_stdout(&mut missing, false).expect("test precondition");
    assert_eq!(missing, b"profile output without marker");

    let mut framed = b"profile output\nshepr-remote-output-ready\nhello\n".to_vec();
    normalize_remote_stdout(&mut framed, true).expect("test precondition");
    assert_eq!(framed, b"hello\n");
}

fn socket_path_byte_len(path: &Path) -> usize {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().len()
}

#[test]
fn local_forward_socket_path_uses_readable_name_when_it_fits() {
    let runtime_dir = shepr_test_support::ScratchDir::new("local-forward-readable");
    // Short target + session leave plenty of room - keep the human-
    // readable form so the socket path stays grep-friendly.
    // remote_bridge_endpoint_path validates this directory as current-user owned
    // mode 0700, so this readable basename is not exposed to other local users.
    let path =
        local_forward_socket_path(runtime_dir.path(), "dev", "default").expect("test precondition");
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    assert!(
        filename.starts_with("shepr-remote-"),
        "expected readable name, got {filename}"
    );
    assert!(filename.contains("-dev-default."), "got {filename}");
    assert!(
        fits_unix_socket_path(&path),
        "socket path too long: {} ({} bytes)",
        path.display(),
        socket_path_byte_len(&path)
    );
}

#[test]
fn local_forward_socket_path_fits_in_sun_path() {
    let runtime_dir = shepr_test_support::ScratchDir::new("lf");
    // The longer readable name falls back to the hashed name when the
    // target and session leave less room beneath the runtime directory.
    let target = "longish-host.example.com";
    let session = "a-fairly-long-session-name-here";
    let path =
        local_forward_socket_path(runtime_dir.path(), target, session).expect("test precondition");
    assert!(
        fits_unix_socket_path(&path),
        "socket path too long for sun_path: {} ({} bytes)",
        path.display(),
        socket_path_byte_len(&path)
    );
}

#[test]
fn local_forward_socket_path_reports_an_overlong_runtime_directory() {
    use std::os::unix::fs::PermissionsExt;

    let scratch = shepr_test_support::ScratchDir::new("local-forward-long-runtime");
    let long_dir = scratch.path().join("a".repeat(80));
    fs::create_dir(&long_dir).expect("test precondition");
    fs::set_permissions(&long_dir, fs::Permissions::from_mode(0o700)).expect("test precondition");

    let error = local_forward_socket_path(&long_dir, "longish-host.example.com", "default")
        .expect_err("socket path cannot fit beneath the runtime directory");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}
