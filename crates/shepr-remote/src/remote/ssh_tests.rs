use super::*;
use shepr_core::socket_path::fits_unix_socket_path;
use std::thread;

/// Paths whose root doubles as the XDG runtime root; the managed config
/// directories are created in the profile runtime directory under it.
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

fn example_target() -> SshTarget {
    SshTarget::parse("example").expect("test precondition")
}

/// A `RemoteSsh` for `example` over a managed config that names no bindable socket.
fn test_ssh() -> RemoteSsh {
    let managed_config =
        write_managed_ssh_config(&example_target(), &test_app_paths(), test_control_dir())
            .expect("test precondition");
    RemoteSsh::test_with_state(
        SshTarget::parse("example").expect("test precondition"),
        managed_config,
    )
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
    let managed_config = write_managed_ssh_config(&example_target(), &paths, test_control_dir())
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
    let keepalive_config = crate::limits::SSH_KEEPALIVE.config_lines();
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
    let first = write_managed_ssh_config(&example_target(), &paths, control_dir)
        .expect("test precondition");
    let second = write_managed_ssh_config(&example_target(), &paths, control_dir)
        .expect("test precondition");
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
    assert_eq!(
        config_path.parent().and_then(Path::parent),
        Some(paths.runtime_dir())
    );
    assert!(
        std::fs::metadata(paths.runtime_dir())
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
fn ssh_failure_output_wins_over_a_stdin_write_error() {
    use std::os::unix::process::ExitStatusExt as _;

    let failure = Output {
        status: std::process::ExitStatus::from_raw(255 << 8),
        stdout: Vec::new(),
        stderr: b"user@host: Permission denied (publickey).".to_vec(),
    };
    let output = finish_ssh_command(
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdin closed")),
        failure,
    )
    .expect("ssh failure output is retained");
    let error = command_failed("remote SSH connection failed", &output);
    assert!(
        crate::SshFailureDiagnostic::from_error(&error).requires_authentication(),
        "the authentication class must survive the failed stdin write"
    );

    let success = Output {
        status: std::process::ExitStatus::from_raw(0),
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    let error = finish_ssh_command(
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdin closed")),
        success,
    )
    .expect_err("a write failure still matters after a successful ssh command");
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
}

#[test]
fn bridge_options_keep_temporary_config_alive_after_helper_drop() {
    let paths = test_app_paths();
    let config = write_managed_ssh_config(&example_target(), &paths, test_control_dir())
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
    let config = write_managed_ssh_config(&example_target(), &paths, test_control_dir())
        .expect("test precondition");
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
        crate::limits::SSH_CONTROL_PERSIST_OPTION,
        ssh_options::BATCH_MODE_NO,
        ssh_options::STRICT_HOST_KEY_CHECKING,
        ssh_options::REMOTE_COMMAND_NONE,
        ssh_options::LOG_LEVEL_ERROR,
        crate::limits::SSH_AUTHENTICATION_PASSWORD_PROMPTS_OPTION,
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
fn ssh_config_quote_wraps_path_with_spaces() {
    assert_eq!(
        ssh_config_quote("/home/a b/.ssh/config"),
        "\"/home/a b/.ssh/config\""
    );
}

#[test]
fn remote_ssh_command_uses_managed_config_when_present() {
    let paths = test_app_paths();
    let managed_config = write_managed_ssh_config(&example_target(), &paths, test_control_dir())
        .expect("write managed config");
    let config_path = managed_config.options.config_path.clone();
    let control_path = managed_config
        .options
        .control_path
        .clone()
        .expect("test precondition");
    let ssh = RemoteSsh::test_with_state(
        SshTarget::parse("example").expect("test precondition"),
        managed_config,
    );

    let command = ssh.command();
    let args = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    assert_eq!(
        args[..9],
        [
            "-C".to_string(),
            "-F".to_string(),
            config_path.to_string_lossy().into_owned(),
            "-S".to_string(),
            control_path.to_string_lossy().into_owned(),
            "-o".to_string(),
            ssh_options::CONTROL_MASTER.to_string(),
            "-o".to_string(),
            crate::limits::SSH_CONTROL_PERSIST_OPTION.to_string(),
        ]
    );
    assert_eq!(&args[args.len() - 2..], ["-T", "example"]);
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
fn ssh_command_cannot_prompt_or_accept_unknown_hosts() {
    let ssh = test_ssh();
    let args = ssh
        .command()
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    for required in [
        "-C",
        ssh_options::BATCH_MODE_YES,
        crate::limits::SSH_NO_PASSWORD_PROMPTS_OPTION,
        ssh_options::STRICT_HOST_KEY_CHECKING,
        ssh_options::REMOTE_COMMAND_NONE,
        ssh_options::LOG_LEVEL_ERROR,
        crate::limits::SSH_CONNECT_TIMEOUT_OPTION,
        crate::limits::SSH_CONNECTION_ATTEMPTS_OPTION,
    ] {
        assert!(args.iter().any(|arg| arg == required), "missing {required}");
    }
    assert!(
        args.iter().all(|arg| !arg.starts_with("ServerAlive")),
        "keepalive settings must come from the managed config, not command-line overrides"
    );
    assert!(
        args.iter().any(|arg| arg == "-F"),
        "missing the managed config"
    );
}

#[test]
fn ssh_modes_override_user_remote_command_and_quiet_logging() {
    let paths = test_app_paths();
    let home = paths.home_dir().expect("test home is configured");
    let user_config = home.join(".ssh").join("config");
    std::fs::create_dir_all(user_config.parent().expect("config has a parent"))
        .expect("create user ssh config directory");
    std::fs::write(
        &user_config,
        "Host example\n  RemoteCommand whoami\n  LogLevel QUIET\n",
    )
    .expect("write user ssh config");
    let target = SshTarget::parse("example").expect("test precondition");
    let config = write_managed_ssh_config(&example_target(), &paths, test_control_dir())
        .expect("write managed config");
    let managed_contents =
        std::fs::read_to_string(&config.options.config_path).expect("read managed config");
    // The managed config includes the user's file, whose RemoteCommand and
    // LogLevel would apply unless the command line overrides them.
    assert!(managed_contents.contains("Include"));
    assert!(managed_contents.contains(&*user_config.to_string_lossy()));
    let ssh = RemoteSsh::test_with_state(target.clone(), config);
    let batch_args = ssh
        .command()
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let auth_config = write_managed_ssh_config(&example_target(), &paths, test_control_dir())
        .expect("write config");
    let auth_args = authentication_command_with_config(&target, auth_config)
        .command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    for args in [&batch_args, &auth_args] {
        for required in [
            ssh_options::STRICT_HOST_KEY_CHECKING,
            ssh_options::REMOTE_COMMAND_NONE,
            ssh_options::LOG_LEVEL_ERROR,
        ] {
            assert!(args.iter().any(|arg| arg == required), "missing {required}");
        }
    }
    assert!(
        batch_args
            .iter()
            .any(|arg| arg == ssh_options::BATCH_MODE_YES)
    );
    assert!(
        auth_args
            .iter()
            .any(|arg| arg == ssh_options::BATCH_MODE_NO)
    );
}

#[test]
fn missing_local_ssh_is_reported_as_local_setup_not_remote_incompatibility() {
    let error = crate::local_setup_error(
        "could not start local ssh",
        io::Error::from(io::ErrorKind::NotFound),
    );
    let diagnostic = crate::SshFailureDiagnostic::from_error(&error);
    assert!(diagnostic.is_local_setup_failure());
    assert!(!diagnostic.is_remote_compatibility());
    assert!(diagnostic.needs_attention());
    assert!(error.to_string().contains("local ssh"));
    assert!(matches!(
        crate::classify_check(Err(error)),
        crate::MachineCheck::Failed(_)
    ));
}

#[test]
fn local_setup_diagnostics_require_an_explicit_local_boundary() {
    for kind in [
        io::ErrorKind::InvalidInput,
        io::ErrorKind::NotFound,
        io::ErrorKind::PermissionDenied,
    ] {
        let error = io::Error::new(kind, "operation failed");
        let untyped = crate::SshFailureDiagnostic::from_error(&error);
        assert!(
            !untyped.needs_attention(),
            "an unwrapped {kind} does not prove a local setup failure"
        );

        let local = crate::SshFailureDiagnostic::from_local_setup_error(&error);
        assert!(local.is_local_setup_failure(), "{kind}");
        assert!(local.needs_attention(), "{kind}");
    }
}

#[test]
fn an_attempt_deadline_shortens_and_then_refuses_commands() {
    let mut ssh = test_ssh();
    let now = Instant::now();
    let timeout = ssh.command_timeout(now).expect("no deadline");
    assert_eq!(timeout.duration, SSH_COMMAND_TIMEOUT);
    assert!(timeout.authentication_candidate);

    ssh.set_attempt_deadline(Some(now + Duration::from_secs(2)));
    let timeout = ssh.command_timeout(now).expect("time is left");
    assert_eq!(timeout.duration, Duration::from_secs(2));
    assert!(!timeout.authentication_candidate);

    ssh.set_attempt_deadline(Some(now + Duration::from_secs(25)));
    let timeout = ssh.command_timeout(now).expect("a round trip fits");
    assert_eq!(timeout.duration, SSH_COMMAND_TIMEOUT);
    assert!(timeout.authentication_candidate);

    ssh.set_attempt_deadline(Some(now));
    let error = ssh
        .command_timeout(now)
        .expect_err("no command may start past the deadline");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    // Treated as a dropped link: no rediscovery, and a retry rather than attention.
    assert!(failed_before_remote_result(&error));
    assert!(!crate::SshFailureDiagnostic::from_error(&error).needs_attention());
    // The refusal happens before ssh is spawned.
    let error = ssh
        .sh_output(&PosixScript::new("true\n"))
        .expect_err("refused");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
}

#[test]
fn a_round_trip_timeout_can_prompt_but_an_attempt_deadline_stays_offline() {
    let round_trip = classify_command_timeout(
        io::Error::new(io::ErrorKind::TimedOut, "SSH command timed out"),
        true,
    );
    let diagnostic = crate::SshFailureDiagnostic::from_error(&round_trip);
    assert!(diagnostic.is_authentication_wait_timeout());
    assert!(diagnostic.failed_before_remote_result());
    assert!(!diagnostic.is_transient_network_failure());

    let attempt = classify_command_timeout(
        io::Error::new(io::ErrorKind::TimedOut, "SSH command timed out"),
        false,
    );
    let diagnostic = crate::SshFailureDiagnostic::from_error(&attempt);
    assert!(!diagnostic.is_authentication_wait_timeout());
    assert!(diagnostic.is_transient_network_failure());
}
