use super::process::{PipeCapture, PipeEcho};
use super::*;
use interprocess::TryClone as _;
use interprocess::local_socket::traits::{Listener as _, Stream as _};
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
mod tests {
    use super::*;

    /// Paths whose root doubles as the XDG runtime directory the managed
    /// config directories are created in.
    fn test_app_paths() -> shepr_config::AppPaths {
        let root = shepr_test_support::ScratchDir::new("remote-ssh");
        shepr_config::AppPaths::rooted_at(&root, Some(&root), None)
    }

    /// The control socket's directory. These tests render config text and
    /// never bind the socket, so it names a directory as short as a real
    /// `/run/user/<uid>`, which no scratch directory under the build tree is.
    fn test_control_dir() -> SshControlDir<'static> {
        SshControlDir::unchecked(Path::new("/nonexistent/ssh"))
    }

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

    #[test]
    fn managed_ssh_config_includes_user_config_then_fallback() {
        use std::os::unix::fs::PermissionsExt;

        let paths = test_app_paths();
        let home = paths.home_dir().expect("test home is configured");
        let user_config = home.join(".ssh").join("config");
        std::fs::create_dir_all(user_config.parent().expect("config has a parent"))
            .expect("create user ssh config directory");
        std::fs::write(&user_config, "Host example\n  ServerAliveInterval 30\n")
            .expect("write user ssh config");
        let managed_config = write_managed_ssh_config("example", &paths, test_control_dir())
            .expect("write managed config");
        let path = managed_config.options.config_path.clone();
        let control_path = managed_config
            .options
            .control_path
            .clone()
            .expect("Unix managed config has a control path");
        let contents = std::fs::read_to_string(&path).expect("read keepalive config");

        // shepr's fallback transport settings are present...
        assert!(
            contents.contains("Host *"),
            "config should add a Host * fallback block: {contents}"
        );
        let keepalive_config = ssh_options::KEEPALIVE.config_lines();
        assert!(
            contents.contains(&keepalive_config),
            "config should set the keepalive values: {contents}"
        );
        assert!(!contents.contains("ControlMaster"));
        assert!(!contents.contains("ControlPersist"));
        assert!(!contents.contains("ControlPath"));
        // ...and any user config is Included (quoted) before it so
        // first-value-wins keeps the user's own settings.
        assert!(
            ssh_config_include(Some(user_config.as_path()))
                .expect("stat user ssh config")
                .is_some(),
            "test user config must be included"
        );
        let include = format!(
            "Include {}",
            ssh_config_quote(&user_config.to_string_lossy())
        );
        let include_at = contents.find(&include).expect("user config Included");
        let fallback_at = contents.find("Host *").expect("fallback present");
        assert!(
            include_at < fallback_at,
            "user config must be Included before shepr's fallback: {contents}"
        );

        let mode = std::fs::metadata(&path)
            .expect("test precondition")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "keepalive config must be user-only");
        // The config lives in a private 0700 dir, not a predictable temp path.
        let dir = path.parent().expect("config has a parent dir");
        let dir_mode = std::fs::metadata(dir)
            .expect("test precondition")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "ssh config dir must be user-only");
        assert!(
            fits_unix_socket_path(&control_path),
            "control socket path must fit portable Unix socket limits"
        );

        drop(managed_config);
    }

    #[test]
    fn shared_ssh_transport_survives_helper_config_drop() {
        let paths = test_app_paths();
        let control_dir = test_control_dir();
        let first =
            write_managed_ssh_config("example", &paths, control_dir).expect("test precondition");
        let second =
            write_managed_ssh_config("example", &paths, control_dir).expect("test precondition");
        let socket = first
            .options
            .control_path
            .clone()
            .expect("test precondition");
        assert_eq!(Some(&socket), second.options.control_path.as_ref());
        assert_eq!(socket.parent(), Some(Path::new("/nonexistent/ssh")));
        assert_ne!(socket.parent(), first.options.config_path.parent());
        let config_path = first.options.config_path.clone();
        drop(first);
        assert!(!config_path.try_exists().expect("stat dropped config"));
        // Dropping one helper's config removes only its own directory: the
        // runtime directory the configs live in, and the other helper's
        // config, stay.
        assert!(
            std::fs::metadata(paths.xdg_runtime_dir())
                .expect("stat runtime directory")
                .is_dir()
        );
        assert!(
            std::fs::metadata(&second.options.config_path)
                .expect("stat surviving config")
                .is_file()
        );
    }

    #[test]
    fn ssh_authentication_diagnostics_are_narrow() {
        let requires_authentication = |message: &str| {
            crate::SshFailureDiagnostic::from_ssh_output(
                Some(crate::SSH_OWN_FAILURE_EXIT_CODE),
                message.into(),
            )
            .requires_authentication()
        };
        for message in [
            "user@host: Permission denied (publickey).",
            "Permission denied (keyboard-interactive,password).",
            "Permission denied (password).",
            "sign_and_send_pubkey: signing failed for ED25519 from agent: agent refused operation",
        ] {
            assert!(requires_authentication(message), "{message}");
        }
        for message in [
            "Host key verification failed.",
            "REMOTE HOST IDENTIFICATION HAS CHANGED!",
            "Permission denied opening /tmp/file",
            "Connection refused",
            "agent disconnected",
            "Permission denied (publickey). Host key verification failed.",
        ] {
            assert!(!requires_authentication(message), "{message}");
        }
    }

    #[test]
    fn bridge_options_keep_temporary_config_alive_after_helper_drop() {
        let paths = test_app_paths();
        let config = write_managed_ssh_config("example", &paths, test_control_dir())
            .expect("test precondition");
        let path = config.options.config_path.clone();
        let worker_options = config.options.clone();
        drop(config);
        assert!(std::fs::metadata(&path).expect("stat config").is_file());
        drop(worker_options);
        assert!(!path.try_exists().expect("stat dropped config"));
    }

    #[test]
    fn authentication_command_uses_shared_transport_without_askpass_or_host_key_relaxation() {
        let paths = test_app_paths();
        let control_dir = test_control_dir();
        let config =
            write_managed_ssh_config("example", &paths, control_dir).expect("test precondition");
        let setup = RemoteSsh::with_control_dir(
            super::super::SshTarget::parse("example").expect("test precondition"),
            Some(control_dir),
            "other-session".into(),
            &paths,
        )
        .expect("managed SSH setup");
        assert_eq!(
            config.options.control_path,
            setup.options().expect("test precondition").control_path
        );
        let authentication = authentication_command_with_config(
            &SshTarget::parse("example").expect("test precondition"),
            config,
        );
        let command = &authentication.command;
        assert_eq!(command.get_program(), "ssh");
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>();
        for required in [
            ssh_options::CONTROL_MASTER,
            ssh_options::CONTROL_PERSIST,
            ssh_options::BATCH_MODE_NO,
            ssh_options::STRICT_HOST_KEY_CHECKING,
            ssh_options::AUTHENTICATION_PASSWORD_PROMPTS,
        ] {
            assert!(args.iter().any(|arg| arg == required), "missing {required}");
        }
        assert_eq!(&args[args.len() - 3..], &["-T", "example", "exit"]);
        let env = command.get_envs().collect::<Vec<_>>();
        assert!(env.iter().any(
            |(key, value)| *key == std::ffi::OsStr::new("SSH_ASKPASS_REQUIRE")
                && *value == Some(std::ffi::OsStr::new("never"))
        ));
        assert!(
            env.iter()
                .any(|(key, value)| *key == std::ffi::OsStr::new("SSH_ASKPASS") && value.is_none())
        );
    }

    #[test]
    fn unmanaged_ssh_setup_preserves_plain_transport() {
        let paths = test_app_paths();
        let ssh = RemoteSsh::new(
            super::super::SshTarget::parse("example").expect("test precondition"),
            false,
            "main".into(),
            &paths,
        )
        .expect("plain SSH setup");
        assert!(ssh.options().is_none());
        assert!(!ssh.command().get_args().any(|arg| arg == "-F"));
    }

    #[test]
    fn ssh_config_quote_wraps_path_with_spaces() {
        assert_eq!(
            ssh_config_quote("/home/a b/.ssh/config"),
            "\"/home/a b/.ssh/config\""
        );
    }

    #[test]
    fn remote_ssh_command_uses_managed_config_when_present() {
        let paths = test_app_paths();
        let managed_config = write_managed_ssh_config("example", &paths, test_control_dir())
            .expect("write managed config");
        let config_path = managed_config.options.config_path.clone();
        let control_path = managed_config
            .options
            .control_path
            .clone()
            .expect("test precondition");
        let ssh = RemoteSsh::test_with_state(
            SshTarget::parse("example").expect("test precondition"),
            shepr_config::DEFAULT_SESSION_NAME.into(),
            Some(managed_config),
            false,
        );

        let command = ssh.command();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            args,
            vec![
                "-C".to_string(),
                "-F".to_string(),
                config_path.to_string_lossy().into_owned(),
                "-S".to_string(),
                control_path.to_string_lossy().into_owned(),
                "-o".to_string(),
                ssh_options::CONTROL_MASTER.to_string(),
                "-o".to_string(),
                ssh_options::CONTROL_PERSIST.to_string(),
                "-T".to_string(),
                "example".to_string(),
            ]
        );
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
        let missing =
            ssh_bridge_exit_error(exit_status(127), b"sh: 1: exec: /old/shepr: not found");
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
    fn exit_sweep_removes_what_owners_left_behind() {
        let registry: &'static TeardownRegistry = Box::leak(Box::new(TeardownRegistry::new()));
        let root = shepr_test_support::ScratchDir::new("teardown");
        let leaked = root.join("leaked");
        let released = root.join("released");
        fs::create_dir_all(&leaked).expect("test precondition");
        fs::create_dir_all(&released).expect("test precondition");

        // An owner still alive at exit (a writer thread that has not run its drop yet).
        let _stuck = registry.register(TeardownResource::Directory(leaked.clone()));
        // An owner that finished its own teardown: it is no longer the sweep's business.
        drop(registry.register(TeardownResource::Directory(released.clone())));

        let started = Instant::now();
        registry.release_all(Duration::from_millis(50));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            !leaked.try_exists().expect("stat leaked directory"),
            "the sweep removes what is still registered"
        );
        assert!(
            released.try_exists().expect("stat released directory"),
            "deregistered resources are left alone"
        );
    }

    #[test]
    fn exit_sweep_waits_for_owners_that_are_already_dropping() {
        let registry: &'static TeardownRegistry = Box::leak(Box::new(TeardownRegistry::new()));
        let registration = registry.register(TeardownResource::Directory(PathBuf::from(
            "/nonexistent/shepr-teardown-test",
        )));
        let owner = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            drop(registration);
        });
        let started = Instant::now();
        registry.release_all(Duration::from_secs(5));
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "returns as soon as the owner deregisters"
        );
        owner.join().expect("test precondition");
    }

    #[test]
    fn noninteractive_ssh_stderr_capture_is_bounded() {
        let stderr = vec![b'x'; NONINTERACTIVE_SSH_STDERR_LIMIT + 4096];
        let captured = PipeCapture::spawn(
            io::Cursor::new(stderr),
            NONINTERACTIVE_SSH_STDERR_LIMIT,
            PipeEcho::None,
        )
        .finish(Duration::from_secs(3))
        .expect("capture stderr");
        assert_eq!(captured.len(), NONINTERACTIVE_SSH_STDERR_LIMIT);
    }

    #[test]
    fn noninteractive_ssh_command_cannot_prompt_or_accept_unknown_hosts() {
        let paths = test_app_paths();
        let ssh = RemoteSsh::new_noninteractive_with(
            super::super::SshTarget::parse("example").expect("test precondition"),
            false,
            &paths,
        )
        .expect("plain SSH setup");
        let args = ssh
            .command()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let keepalive_options = ssh_options::KEEPALIVE.command_options();
        for required in [
            "-C",
            ssh_options::BATCH_MODE_YES,
            ssh_options::NONINTERACTIVE_PASSWORD_PROMPTS,
            ssh_options::STRICT_HOST_KEY_CHECKING,
            ssh_options::CONNECT_TIMEOUT,
            ssh_options::CONNECTION_ATTEMPTS,
            keepalive_options[0].as_str(),
            keepalive_options[1].as_str(),
        ] {
            assert!(args.iter().any(|arg| arg == required), "missing {required}");
        }
        assert_eq!(args.iter().any(|arg| arg == "-F"), ssh.options().is_some());
    }

    #[test]
    fn remote_setup_approval_requires_input_and_rejects_unrecognized_answers() {
        for default in [false, true] {
            for input in ["", "maybe\n"] {
                assert_eq!(
                    read_remote_confirmation(&mut input.as_bytes(), default)
                        .expect_err("test precondition")
                        .kind(),
                    io::ErrorKind::Interrupted
                );
            }
            assert_eq!(
                read_remote_confirmation(&mut "\n".as_bytes(), default).expect("test precondition"),
                default
            );
            assert!(
                read_remote_confirmation(&mut "YES\n".as_bytes(), default)
                    .expect("test precondition")
            );
            assert!(
                !read_remote_confirmation(&mut "no\n".as_bytes(), default)
                    .expect("test precondition")
            );
        }
    }

    #[test]
    fn saved_machine_server_commands_are_scoped_to_the_explicit_session() {
        let shepr = RemoteExecutable::parse("/usr/bin/shepr").expect("test precondition");
        for (args, command) in [
            (&["status", "server", "--json"][..], "status server --json"),
            (&["server", "stop"][..], "server stop"),
            (&["remote-client-bridge"][..], "remote-client-bridge"),
        ] {
            assert_eq!(
                shepr.session_command("agents", args),
                format!("{} --session agents {command}", shepr.as_str())
            );
            assert_eq!(
                shepr.session_command(shepr_config::DEFAULT_SESSION_NAME, args),
                format!("{} {command}", shepr.as_str())
            );
        }
    }

    #[test]
    fn remote_ssh_commands_compress_without_managed_config() {
        let ssh = RemoteSsh::test_with_state(
            SshTarget::parse("example").expect("test precondition"),
            shepr_config::DEFAULT_SESSION_NAME.into(),
            None,
            false,
        );

        let command = ssh.command();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(args, vec!["-C", "-T", "example"]);
    }

    #[test]
    fn an_attempt_deadline_shortens_and_then_refuses_noninteractive_commands() {
        let mut ssh = RemoteSsh::test_with_state(
            SshTarget::parse("example").expect("test precondition"),
            shepr_config::DEFAULT_SESSION_NAME.into(),
            None,
            true,
        );
        assert_eq!(
            ssh.noninteractive_timeout().expect("no deadline"),
            NONINTERACTIVE_SSH_COMMAND_TIMEOUT
        );

        ssh.set_attempt_deadline(Some(Instant::now() + Duration::from_secs(2)));
        let timeout = ssh.noninteractive_timeout().expect("time is left");
        assert!(timeout <= Duration::from_secs(2), "{timeout:?}");

        ssh.set_attempt_deadline(Some(Instant::now()));
        let error = ssh
            .noninteractive_timeout()
            .expect_err("no command may start past the deadline");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        // Treated as a dropped link: no rediscovery, and a retry rather than attention.
        assert!(is_ssh_link_failure(&error));
        assert!(!crate::SshFailureDiagnostic::from_error(&error).needs_attention());
        // The refusal happens before ssh is spawned.
        let error = ssh.sh_output("true\n").expect_err("refused");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn sanitize_path_component_removes_shell_sensitive_chars() {
        assert_eq!(sanitize_path_component("user@host:22"), "user-host-22");
    }

    #[test]
    fn remote_executable_rejects_paths_that_need_shell_quoting() {
        for path in [
            "/home/user's files/shepr",
            "/home/$literal/shepr",
            "/opt/shepr bin/shepr",
        ] {
            assert!(RemoteExecutable::parse(path).is_err(), "{path}");
        }
        let path = "/home/user/.local/bin/shepr-0.1+dev";
        let resolved = RemoteExecutable::parse(path).expect("test precondition");
        assert_eq!(resolved.as_str(), path);
        assert_eq!(resolved.quoted(), shell_quote(path));
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

    #[test]
    fn reattach_command_includes_remote_and_session() {
        assert_eq!(
            reattach_command(
                "target/release/shepr",
                "user@host",
                "work",
                RemoteKeybindings::Local,
            ),
            "target/release/shepr --remote user@host --session work"
        );
        assert_eq!(
            reattach_command(
                "shepr",
                "host name",
                shepr_config::DEFAULT_SESSION_NAME,
                RemoteKeybindings::Local,
            ),
            "shepr --remote 'host name'"
        );
        assert_eq!(
            reattach_command(
                "shepr",
                "host",
                shepr_config::DEFAULT_SESSION_NAME,
                RemoteKeybindings::Server,
            ),
            "shepr --remote host --remote-keybindings server"
        );
    }

    #[test]
    fn remote_bridge_command_passes_a_named_session() {
        let remote = RemoteExecutable::parse("/usr/bin/shepr").expect("test precondition");
        assert!(
            remote
                .bridge_command("agents")
                .contains(" --session agents remote-client-bridge; shepr_exit_status=")
        );
    }

    #[test]
    fn remote_bridge_command_uses_installed_binary() {
        let remote_shepr = RemoteExecutable::parse("/usr/bin/shepr").expect("test precondition");
        assert_eq!(
            remote_shepr.bridge_command(shepr_config::DEFAULT_SESSION_NAME),
            format!(
                "/bin/sh -c 'echo; echo shepr-remote-output-ready; /usr/bin/shepr remote-client-bridge; shepr_exit_status=$?; if [ $shepr_exit_status -eq {SSH_OWN_FAILURE_EXIT_CODE} ]; then exit {REMAPPED_REMOTE_255_EXIT_CODE}; fi; exit $shepr_exit_status'"
            )
        );
        assert_eq!(
            remote_shepr.saved_bridge_command("agents"),
            "/usr/bin/shepr --session agents remote-client-bridge </dev/null"
        );
    }

    #[test]
    fn remote_path_discovery_uses_path_binary() {
        let remote_shepr =
            remote_executable_from_path_discovery("/usr/bin/shepr\n").expect("path binary");

        assert_eq!(
            remote_shepr.bridge_command(shepr_config::DEFAULT_SESSION_NAME),
            format!(
                "/bin/sh -c 'echo; echo shepr-remote-output-ready; /usr/bin/shepr remote-client-bridge; shepr_exit_status=$?; if [ $shepr_exit_status -eq {SSH_OWN_FAILURE_EXIT_CODE} ]; then exit {REMAPPED_REMOTE_255_EXIT_CODE}; fi; exit $shepr_exit_status'"
            )
        );
    }

    #[test]
    fn remote_path_discovery_ignores_binaries_that_need_quoting() {
        // The bridge script must reach the login shell as one quoted word
        // with no quote inside, so a path needing quotes cannot be used.
        for discovered in ["/opt/shepr bin/shepr\n", "/opt/shepr's/bin/shepr\n"] {
            assert!(
                remote_executable_from_path_discovery(discovered).is_none(),
                "{discovered:?}"
            );
        }
    }

    /// The bridge command is interpreted by /bin/sh, not by the login shell: the
    /// login shell only sees `/bin/sh -c` and one quoted word without newlines.
    #[test]
    fn saved_bridge_command_does_not_depend_on_a_posix_login_shell() {
        let remote = RemoteExecutable::parse("/usr/bin/shepr").expect("test precondition");
        let command = remote.bridge_command("agents");
        let script = command
            .strip_prefix("/bin/sh -c '")
            .and_then(|rest| rest.strip_suffix('\''))
            .expect("wrapped in /bin/sh -c");
        assert!(!script.contains('\''), "{script}");
        assert!(!script.contains('\n'), "{script}");

        // The script, run by a real /bin/sh, still frames its output with the marker.
        // host-program-ok: the generated remote script is the subject, run as sshd runs it
        let output = shepr_test_support::command_in_scratch("/bin/sh", "saved-bridge-command-sh")
            .arg("-c")
            .arg(posix_remote_output_command("printf payload"))
            .output()
            .expect("test precondition");
        let mut stdout = output.stdout;
        normalize_remote_stdout(&mut stdout, output.status.success()).expect("marker line present");
        assert_eq!(stdout, b"payload");
    }

    /// `sh_output` scripts end with a newline and are fed to `/bin/sh -s`; the
    /// wrapper must stay valid shell for them and keep 255 for ssh's own failures.
    #[test]
    fn remote_output_wrapper_accepts_newline_scripts_and_remaps_exit_255() {
        use std::io::Write as _;

        let run = |script: &str| {
            // host-program-ok: the generated remote script is the subject, run as sshd runs it
            let mut child =
                shepr_test_support::command_in_scratch("/bin/sh", "remote-output-wrapper-sh")
                    .arg("-s")
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .expect("test precondition");
            child
                .stdin
                .take()
                .expect("test precondition")
                .write_all(posix_remote_output_command(script).as_bytes())
                .expect("test precondition");
            child.wait_with_output().expect("test precondition")
        };

        let output = run("printf payload\n");
        assert!(output.status.success(), "{output:?}");
        let mut stdout = output.stdout;
        normalize_remote_stdout(&mut stdout, true).expect("marker line present");
        assert_eq!(stdout, b"payload");

        assert_eq!(
            run(&known_remote_binary_candidate_script()).status.code(),
            Some(0)
        );
        assert_eq!(run("exit 3\n").status.code(), Some(3));
        let remote_ssh_status = format!("(exit {SSH_OWN_FAILURE_EXIT_CODE})\n");
        assert_eq!(
            run(&remote_ssh_status).status.code(),
            Some(REMAPPED_REMOTE_255_EXIT_CODE)
        );
    }

    /// The cached API-bridge command reaches the login shell as `/bin/sh -c`
    /// plus one single-quoted word with no quote, backslash or newline inside,
    /// and both of its branches behave when a real /bin/sh runs it.
    #[test]
    fn cached_api_command_does_not_depend_on_a_posix_login_shell() {
        use shepr_test_support::fixture::{self, Step};

        // The remote shepr: a fixture stand-in that passes the bridge check
        // and otherwise answers as the bridge, naming the session it was given.
        let dir = shepr_test_support::ScratchDir::new("api-command");
        let check = ["--session", "agents", "remote-api-bridge", "--check"];
        let fake = fixture::stand_in(
            &dir,
            "shepr",
            &[
                Step::When {
                    operands: check.map(String::from).to_vec(),
                    steps: vec![Step::Exit(0)],
                },
                Step::Print("bridged-".into()),
                Step::PrintArg(2),
                Step::Print("\n".into()),
            ],
        );

        let run = |executable: &str| {
            let executable =
                RemoteExecutable::parse(executable.to_owned()).expect("test precondition");
            let command = cached_remote_api_command(&executable, "agents");
            let script = command
                .strip_prefix("/bin/sh -c '")
                .and_then(|rest| rest.strip_suffix('\''))
                .expect("wrapped in /bin/sh -c")
                .to_owned();
            for forbidden in ['\'', '\n', '\\', '"'] {
                assert!(!script.contains(forbidden), "{forbidden:?} in {script}");
            }
            // host-program-ok: the generated remote script is the subject, run as sshd runs it
            shepr_test_support::command_in_scratch("/bin/sh", "cached-api-command-sh")
                .arg("-c")
                .arg(&script)
                .stdin(std::process::Stdio::null())
                .output()
                .expect("test precondition")
        };

        let fake_path = fake.to_str().expect("test precondition").to_owned();
        let output = run(&fake_path);
        let mut stdout = output.stdout;
        assert!(output.status.success());
        normalize_remote_stdout(&mut stdout, true).expect("marker line present");
        assert_eq!(stdout, b"bridged-agents\n");

        let output = run("/nonexistent/shepr");
        assert_eq!(output.status.code(), Some(78));
        assert!(String::from_utf8_lossy(&output.stderr).contains(STALE_API_METADATA));
    }

    #[test]
    fn remote_path_discovery_reads_multiple_absolute_paths() {
        let candidates = remote_executables_from_path_discovery(
            "/usr/bin/shepr\nbin/shepr\n /opt/shepr-bin/shepr\n",
        );

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].as_str(), "/usr/bin/shepr");
        assert_eq!(candidates[1].as_str(), "/opt/shepr-bin/shepr");
    }

    #[test]
    fn remote_path_discovery_ignores_mise_shims() {
        let candidates = remote_executables_from_path_discovery(
            "/home/can/.local/share/mise/shims/shepr\n/home/can/.local/share/mise/installs/shepr/0.7.1/bin/shepr\n",
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].as_str(),
            "/home/can/.local/share/mise/installs/shepr/0.7.1/bin/shepr"
        );
    }

    #[test]
    fn remote_path_discovery_only_accepts_cacheable_executables() {
        let too_long = format!("/{}/shepr", "a".repeat(4090));
        let output = format!("/opt/shepr\u{1}\n{too_long}\n/usr/bin/shepr\n");
        let candidates = remote_executables_from_path_discovery(&output);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].as_str(), "/usr/bin/shepr");
    }

    #[test]
    fn known_remote_binary_candidate_script_includes_cargo_and_local_bin() {
        let script = known_remote_binary_candidate_script();

        assert!(script.contains("emit \"$home/.cargo/bin/shepr\""));
        assert!(script.contains("emit \"$home/.local/bin/shepr\""));
    }

    #[test]
    fn api_forwarding_check_runs_the_candidate_for_the_session() {
        let remote_shepr =
            remote_executable_from_path_discovery("/home/u/.cargo/bin/shepr\n").expect("path");

        assert_eq!(
            remote_shepr.api_bridge_check_command("agents"),
            "test -x /home/u/.cargo/bin/shepr && /home/u/.cargo/bin/shepr status client --json && /home/u/.cargo/bin/shepr --session agents remote-api-bridge --check </dev/null"
        );
    }

    #[test]
    fn remote_path_discovery_ignores_relative_paths() {
        let remote_shepr = remote_executable_from_path_discovery("bin/shepr\n");

        assert!(remote_shepr.is_none());
    }

    #[test]
    fn remote_path_discovery_ignores_empty_output() {
        let remote_shepr = remote_executable_from_path_discovery("\n");

        assert!(remote_shepr.is_none());
    }

    #[test]
    fn parse_client_status_json_reads_last_json_record() {
        let status = parse_client_status_json(
            "wrapper output\n{\"version\":\"0.8.0\",\"build_id\":\"0123456789abcdef\"}\n{\"wrapper\":true}\n",
        )
        .expect("test precondition");
        assert_eq!(status.version.as_deref(), Some("0.8.0"));
        assert_eq!(status.build_id.as_deref(), Some("0123456789abcdef"));
    }

    #[test]
    fn parse_remote_server_status_json_reads_running_server() {
        assert_eq!(
            parse_remote_server_status_json(
                r#"{"status":"running","running":true,"version":"0.6.0","build_id":"0123456789abcdef","capabilities":{"detached_server_daemon":true,"ssh_agent_registration":false}}"#
            )
            .expect("test precondition"),
            RemoteServerStatus::Running {
                version: Some("0.6.0".into()),
                build_id: Some("0123456789abcdef".into()),
                detached_server_daemon: true
            }
        );
    }

    #[test]
    fn parse_remote_server_status_json_reads_stopped_server() {
        assert_eq!(
            parse_remote_server_status_json(
                r#"{"status":"not_running","running":false,"version":null}"#
            )
            .expect("test precondition"),
            RemoteServerStatus::NotRunning
        );
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
        let path = local_forward_socket_path(runtime_dir.path(), "dev", "default")
            .expect("test precondition");
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
        let path = local_forward_socket_path(runtime_dir.path(), target, session)
            .expect("test precondition");
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
        fs::set_permissions(&long_dir, fs::Permissions::from_mode(0o700))
            .expect("test precondition");

        let error = local_forward_socket_path(&long_dir, "longish-host.example.com", "default")
            .expect_err("socket path cannot fit beneath the runtime directory");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
