use super::*;
use crate::host::classified_bridge_failure;
use crate::ssh::normalize_remote_stdout;
use shepr_launch::{EndpointFailure, FailureCause, FailureDisposition};
use std::io::Read as _;
use std::time::Duration;

impl BridgeUpload {
    /// Stop copying. Bytes already read are still written.
    fn cancel(&self) {
        self.stop.cancel();
    }

    fn is_finished(&self) -> bool {
        self.worker.is_finished()
    }
}

#[test]
fn upload_cancellation_preserves_pending_endpoint_download() {
    let scratch = shepr_test_support::ScratchDir::new("cancel");
    let path = scratch.join("s.sock");
    let listener = shepr_platform::ipc::bind_local_listener(&path).expect("test precondition");
    let mut endpoint = shepr_platform::ipc::connect_local_stream(&path).expect("test precondition");
    let mut bridge = listener.accept().expect("test precondition").0;
    std::fs::remove_file(path).expect("test precondition");
    drop(listener);
    let mut endpoint_reader = endpoint.try_clone().expect("test precondition");

    struct ForwardedInput(std::sync::mpsc::Sender<Vec<u8>>);
    impl io::Write for ForwardedInput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .send(bytes.to_vec())
                .map_err(|error| io::Error::other(error.to_string()))?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let (forwarded_tx, forwarded_rx) = std::sync::mpsc::channel();
    let upload_stream = bridge.try_clone().expect("test precondition");
    upload_stream
        .set_nonblocking(true)
        .expect("test stream supports nonblocking mode");
    let upload = BridgeUpload::spawn(
        upload_stream,
        ForwardedInput(forwarded_tx),
        Arc::new(AtomicBool::new(false)),
    )
    .expect("test bridge upload starts");
    let cancel = move || {
        upload.cancel();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !upload.is_finished() {
            assert!(
                Instant::now() < deadline,
                "upload worker completes within timeout"
            );
            thread::sleep(Duration::from_millis(1));
        }
        let end = upload.join().expect("upload worker does not panic");
        end.result.expect("upload copy completes without error");
        assert!(
            !end.client_closed,
            "upload cancellation must not report peer EOF"
        );
    };

    let first_message = shepr_protocol::ClientMessage::ClientShellFocus { focused: false };
    let mut expected = Vec::new();
    shepr_protocol::write_message(&mut expected, &first_message).expect("test precondition");
    endpoint.write_all(&expected).expect("test precondition");
    let mut forwarded = Vec::new();
    while forwarded.len() < expected.len() {
        forwarded.extend(
            forwarded_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("test precondition"),
        );
    }
    assert_eq!(forwarded, expected);
    cancel();

    // Cancelling uploads leaves the same endpoint stream available in both directions.
    bridge
        .set_nonblocking(false)
        .expect("test stream supports blocking mode");
    let second_message = shepr_protocol::ClientMessage::ClientShellFocus { focused: true };
    shepr_protocol::write_message(&mut endpoint, &second_message).expect("test precondition");
    let received: shepr_protocol::ClientMessage =
        shepr_protocol::read_message(&mut bridge).expect("test precondition");
    assert_eq!(received, second_message);

    const FINAL: &[u8] = b"pending-download: FINAL OUTPUT\n";
    bridge.write_all(FINAL).expect("test precondition");
    drop(bridge);
    let mut output = Vec::new();
    endpoint_reader
        .read_to_end(&mut output)
        .expect("test precondition");
    assert_eq!(output, FINAL);
}

/// Whether `error` came from ssh, the link or a bounded command timeout rather
/// than from a remote command, so nothing is known about the remote install.
fn failed_before_remote_result(error: &io::Error) -> bool {
    SshFailureDiagnostic::from_error(error).failed_before_remote_result()
}

fn upload_test_streams() -> (
    shepr_platform::ipc::LocalStream,
    shepr_platform::ipc::LocalStream,
) {
    let (client, server) = shepr_platform::ipc::LocalStream::pair().expect("test precondition");
    server.set_nonblocking(true).expect("test precondition");
    (client, server)
}

#[test]
fn bridge_download_drain_timeout_does_not_join_and_shuts_down_the_stream() {
    let (mut client, bridge) = upload_test_streams();
    let connection_stop = AtomicBool::new(false);
    let (release_tx, release_rx) = mpsc::channel();
    let download = BridgeDownload::spawn(move || {
        release_rx
            .recv()
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(0)
    });

    let result = download
        .finish(Duration::from_millis(1), &connection_stop, &bridge)
        .expect("a drain timeout is an ordinary bounded end");
    assert!(matches!(result, BridgeDownloadEnd::DrainTimedOut));
    assert!(connection_stop.load(Ordering::Acquire));
    let mut byte = [0_u8; 1];
    release_tx
        .send(())
        .expect("download worker is still waiting");
    assert_eq!(client.read(&mut byte).expect("shutdown reaches peer"), 0);
}

#[test]
fn bridge_upload_idle_waits_without_repeated_reads_and_cancels() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;

    struct CountingUploadStream {
        stream: shepr_platform::ipc::LocalStream,
        polls: Arc<AtomicUsize>,
        waits: mpsc::Sender<()>,
    }

    impl UploadReadStream for CountingUploadStream {
        fn poll_read_count(
            &mut self,
            buffer: &mut [u8],
        ) -> io::Result<shepr_platform::ipc::LocalStreamReadCount> {
            self.polls.fetch_add(1, Ordering::Relaxed);
            shepr_platform::ipc::poll_local_stream_read_count(&mut self.stream, buffer)
        }

        fn wait_for_input(&self, wake: &shepr_platform::StreamWake) -> io::Result<()> {
            self.waits
                .send(())
                .map_err(|error| io::Error::other(error.to_string()))?;
            wake.wait(&self.stream)
        }
    }

    let (mut client, stream) = upload_test_streams();
    let attempts = Arc::new(AtomicUsize::new(0));
    let worker_attempts = Arc::clone(&attempts);
    let stop = Arc::new(BridgeUploadStop::new().expect("test precondition"));
    let worker_stop = Arc::clone(&stop);
    let (waiting_tx, waiting_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut output = Vec::new();
        let closed = AtomicBool::new(false);
        let result = copy_upload_stream_to_writer(
            CountingUploadStream {
                stream,
                polls: worker_attempts,
                waits: waiting_tx,
            },
            &mut output,
            &worker_stop,
            &AtomicBool::new(false),
            &closed,
        );
        done_tx
            .send((result, output, closed.load(Ordering::Acquire)))
            .expect("test precondition");
    });
    waiting_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the empty read reached its blocking wait");
    let idle_reads = attempts.load(Ordering::Relaxed);
    client.write_all(b"pane input").expect("test precondition");
    waiting_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("input woke the upload worker and it returned to its wait");
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

    let (mut client, stream) = upload_test_streams();
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
    let (_client, stream) = upload_test_streams();
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
    let (mut client, stream) = upload_test_streams();
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
fn socket_pair_bridge_relays_the_one_connection_and_returns_ssh_diagnostics() {
    use shepr_test_support::fixture::{self, Step};

    let env = shepr_test_support::IsolatedEnv::new();
    let scratch = shepr_test_support::ScratchDir::new("bridge-pair");
    let echoing_dir = scratch.join("echoing");
    std::fs::create_dir(&echoing_dir).expect("echoing fake SSH directory");
    let failing_dir = scratch.join("failing");
    std::fs::create_dir(&failing_dir).expect("failing fake SSH directory");
    let _echoing = fixture::stand_in(
        &echoing_dir,
        "ssh",
        &[
            Step::Print("shepr-remote-output-ready\n".into()),
            Step::Cat,
            Step::PrintErr("Connection refused\n".into()),
            Step::Exit(255),
        ],
    );
    let _failing = fixture::stand_in(
        &failing_dir,
        "ssh",
        &[
            Step::Print("shepr-remote-output-ready\n".into()),
            Step::PrintErr("Connection refused\n".into()),
            Step::Exit(255),
        ],
    );
    env.set("PATH", &echoing_dir);
    // The managed config lives under its own root, so the entry count of
    // `scratch` below sees only the two stand-in directories.
    let ssh_root = shepr_test_support::ScratchDir::new("bridge-pair-ssh");
    let ssh_paths = shepr_paths::AppPaths::rooted_at(&ssh_root, Some(&ssh_root), None)
        .expect("scratch roots fit a socket");
    let ssh_options = crate::ssh::managed_ssh_options_for_test(
        &SshTarget::parse("example").expect("target"),
        &ssh_paths,
    )
    .expect("managed ssh options");

    let (bridge, mut stream) = SshStdioBridge::start_command(
        SshTarget::parse("example").expect("target"),
        AccountShellCommand::from_account_shell_text("unused"),
        &ssh_options,
    )
    .expect("socket pair bridge");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("read timeout");
    stream.write_all(b"one connection").expect("upload");
    let mut echoed = [0_u8; 14];
    stream.read_exact(&mut echoed).expect("download");
    assert_eq!(&echoed, b"one connection");
    // Ending uploads lets fake SSH publish its diagnostic while downloads drain.
    stream
        .shutdown(std::net::Shutdown::Write)
        .expect("end upload");
    let mut remainder = Vec::new();
    stream.read_to_end(&mut remainder).expect("EOF");
    assert!(remainder.is_empty());
    // A local write EOF is deliberately classified as client closure, so the
    // worker completes normally rather than presenting its exit as a link fault.
    assert!(bridge.reported_failure().is_none());
    drop(bridge);
    // Dropping a live bridge stops its one worker while the endpoint is still
    // open.
    let (idle_bridge, idle_stream) = SshStdioBridge::start_command(
        SshTarget::parse("example").expect("target"),
        AccountShellCommand::from_account_shell_text("unused"),
        &ssh_options,
    )
    .expect("idle socket pair bridge");
    let started = Instant::now();
    drop(idle_bridge);
    assert!(started.elapsed() < Duration::from_secs(3));
    drop(idle_stream);

    // The next connection fails without a client close. Its own worker must
    // return the typed SSH diagnostic after EOF.
    env.set("PATH", &failing_dir);
    let (bridge, mut stream) = SshStdioBridge::start_command(
        SshTarget::parse("example").expect("target"),
        AccountShellCommand::from_account_shell_text("unused"),
        &ssh_options,
    )
    .expect("second socket pair bridge");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("read timeout");
    stream.read_to_end(&mut Vec::new()).expect("failed SSH EOF");
    let failure = bridge.reported_failure().expect("SSH diagnostic");
    assert!(failure.to_string().contains("Connection refused"));
    assert!(failed_before_remote_result(&failure));
    drop(bridge);
    assert_eq!(
        std::fs::read_dir(scratch.path())
            .expect("scratch entries")
            .count(),
        2
    );
}

#[test]
fn bridge_worker_failure_is_returned_once_and_drop_is_safe() {
    let should_stop = Arc::new(AtomicBool::new(false));
    let bridge = SshStdioBridge {
        should_stop,
        worker: std::sync::Mutex::new(Some(thread::spawn(move || {
            Err(local_setup_error(
                "test SSH setup",
                io::Error::other("test failure"),
            ))
        }))),
    };
    let error = bridge.reported_failure().expect("worker diagnostic");
    assert!(SshFailureDiagnostic::from_error(&error).is_local_setup_failure());
    assert!(error.to_string().contains("test failure"));
    assert!(bridge.reported_failure().is_none());
    drop(bridge);
}

fn exit_status(code: i32) -> std::process::ExitStatus {
    use std::os::unix::process::ExitStatusExt as _;
    std::process::ExitStatus::from_raw(code << 8)
}

#[test]
fn only_ssh_own_exit_code_counts_as_failing_before_a_remote_result() {
    let link = ssh_bridge_exit_error(
        exit_status(SSH_OWN_FAILURE_EXIT_CODE),
        b"Connection refused",
    );
    assert!(failed_before_remote_result(&link));
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
    assert!(!failed_before_remote_result(&remapped));
    let remapped_message = remapped.to_string();
    assert!(remapped_message.contains(&format!(
        "remote status {SSH_OWN_FAILURE_EXIT_CODE} is remapped to {REMAPPED_REMOTE_255_EXIT_CODE}"
    )));
    assert!(remapped_message.contains(&format!(
        "a native {REMAPPED_REMOTE_255_EXIT_CODE} is indistinguishable"
    )));
    let missing = ssh_bridge_exit_error(exit_status(127), b"sh: 1: exec: /old/shepr: not found");
    assert!(!failed_before_remote_result(&missing));
    assert_eq!(
        missing.to_string(),
        "remote command failed (exit status 127): sh: 1: exec: /old/shepr: not found"
    );
    assert!(failed_before_remote_result(&io::Error::new(
        io::ErrorKind::TimedOut,
        "handshake timed out"
    )));
    assert!(!failed_before_remote_result(&io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "closed before welcome"
    )));
}

#[test]
fn remote_bridge_failures_need_attention_only_when_the_host_must_be_fixed() {
    use shepr_launch::RemoteFailureClass;
    use shepr_launch::local_server::LaunchError;
    let marker = BRIDGE_FAILURE_MARKER;

    // `classified_bridge_failure` is the production record producer. The
    // `error:` line and optional logging notice are printed by the root CLI,
    // above this crate, so this consumer-boundary test assembles those lines
    // rather than making remote depend on the CLI binary.
    for class in RemoteFailureClass::ALL {
        let disposition = match class {
            RemoteFailureClass::Repair => FailureDisposition::Repair,
            RemoteFailureClass::Retry
            | RemoteFailureClass::NoServer
            | RemoteFailureClass::Stopping => FailureDisposition::Retry,
        };
        // The record as the remote binary prints it: its error line, after a
        // notice the bridge may have printed before failing.
        let record = classified_bridge_failure(
            class,
            io::ErrorKind::Other,
            &"shepr-server refused its configuration\n  invalid server.toml",
        );
        let stderr = format!("shepr: could not initialize file logging\nerror: {record}");
        let error = ssh_bridge_exit_error(exit_status(1), stderr.as_bytes());
        let failure = EndpointFailure::from_error(&error);
        assert_eq!(failure.disposition(), disposition, "{class:?}");
        assert!(failure.to_string().contains("invalid server.toml"));
        assert!(!failure.to_string().contains(marker));
        assert!(!failure.to_string().contains("file logging"));
        // A fault on the remote host is not reported as local setup.
        assert!(
            !SshFailureDiagnostic::from_error(&error).is_local_setup_failure(),
            "{class:?}"
        );
    }

    // CLI stderr prefixes are presentation. A future spelling change must
    // leave the marker record recognizable to the local bridge.
    let record = classified_bridge_failure(
        RemoteFailureClass::Repair,
        io::ErrorKind::Other,
        &"shepr-server refused its configuration\n  invalid server.toml",
    );
    for prefix in ["error: ", "shepr: ", "remote command: "] {
        let error = ssh_bridge_exit_error(exit_status(1), format!("{prefix}{record}").as_bytes());
        assert_eq!(
            EndpointFailure::from_error(&error).disposition(),
            FailureDisposition::Repair,
            "{prefix}"
        );
    }

    // A launch failure keeps the class its LaunchError mapping gave it.
    for (launch_error, disposition) in [
        (
            LaunchError::Executable(io::Error::new(
                io::ErrorKind::NotFound,
                "shepr-server was not found",
            )),
            FailureDisposition::Repair,
        ),
        (
            LaunchError::LaunchLock(io::Error::new(
                io::ErrorKind::TimedOut,
                "another shepr is still starting the server",
            )),
            FailureDisposition::Retry,
        ),
    ] {
        let record = classified_bridge_failure(
            launch_error.remote_failure_class(),
            launch_error.kind(),
            &launch_error,
        );
        let error = ssh_bridge_exit_error(exit_status(1), format!("error: {record}").as_bytes());
        let failure = EndpointFailure::from_error(&error);
        assert_eq!(failure.disposition(), disposition, "{launch_error:?}");
        assert!(failure.to_string().contains(&launch_error.to_string()));
    }

    // A remote that writes no record, or a class this build does not know,
    // degrades to an unclassified retry.
    for stderr in [
        "server config was refused".to_owned(),
        format!("error: {marker}unknown\ninvalid server.toml"),
    ] {
        let error = ssh_bridge_exit_error(exit_status(1), stderr.as_bytes());
        let failure = EndpointFailure::from_error(&error);
        assert_eq!(failure.cause(), FailureCause::Unclassified, "{stderr}");
        assert_eq!(failure.disposition(), FailureDisposition::Retry, "{stderr}");
    }
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
