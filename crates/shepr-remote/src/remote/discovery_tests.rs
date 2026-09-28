//! Full discovery of a saved machine's remote executable, resumed across attempts.

use super::*;

fn executable(path: &'static str) -> RemoteExecutable {
    RemoteExecutable::parse(path).expect("test precondition")
}

/// A remote host reached through fake round trips. `fail_at` names the call (counted
/// from zero over this fake's whole life) that times out, as an attempt deadline would.
struct FakeHost {
    login_shell: Option<&'static str>,
    sh: Option<&'static str>,
    known: Vec<&'static str>,
    matching: &'static str,
    calls: Vec<String>,
    fail_at: Vec<usize>,
    /// Calls that fail with an error other than a link failure.
    fail_other_at: Vec<usize>,
}

impl FakeHost {
    fn new(
        login_shell: Option<&'static str>,
        known: Vec<&'static str>,
        matching: &'static str,
    ) -> Self {
        Self {
            login_shell,
            sh: None,
            known,
            matching,
            calls: Vec::new(),
            fail_at: Vec::new(),
            fail_other_at: Vec::new(),
        }
    }

    fn call(&mut self, name: String) -> io::Result<()> {
        let index = self.calls.len();
        self.calls.push(name);
        if self.fail_at.contains(&index) {
            return Err(attempt_deadline_passed());
        }
        if self.fail_other_at.contains(&index) {
            return Err(io::Error::other(
                "remote binary discovery failed: exit status: 1",
            ));
        }
        Ok(())
    }
}

impl DiscoverySteps for FakeHost {
    fn path_via_login_shell(&mut self) -> io::Result<Option<RemoteExecutable>> {
        self.call("login".into())?;
        Ok(self.login_shell.map(executable))
    }

    fn path_via_sh(&mut self) -> io::Result<Option<RemoteExecutable>> {
        self.call("sh".into())?;
        Ok(self.sh.map(executable))
    }

    fn known_locations(&mut self) -> io::Result<Vec<RemoteExecutable>> {
        self.call("known".into())?;
        Ok(self.known.iter().map(|path| executable(path)).collect())
    }

    fn matches(&mut self, candidate: &RemoteExecutable) -> io::Result<bool> {
        self.call(format!("probe {}", candidate.as_str()))?;
        Ok(candidate.as_str() == self.matching)
    }

    fn target(&self) -> &str {
        "build"
    }
}

#[test]
fn uninterrupted_discovery_runs_each_round_trip_once() {
    let mut host = FakeHost::new(
        Some("/usr/bin/shepr"),
        vec!["/home/u/.cargo/bin/shepr", "/usr/bin/shepr"],
        "/home/u/.cargo/bin/shepr",
    );
    let found = DiscoveryProgress::default()
        .advance(&mut host)
        .expect("a matching candidate is found");
    assert_eq!(found.as_str(), "/home/u/.cargo/bin/shepr");
    // The login shell found a path, so the /bin/sh fallback is skipped, and the
    // duplicate from the known locations is probed once.
    assert_eq!(
        host.calls,
        [
            "login",
            "known",
            "probe /usr/bin/shepr",
            "probe /home/u/.cargo/bin/shepr"
        ]
    );
}

#[test]
fn a_timed_out_attempt_resumes_at_the_round_trip_it_did_not_finish() {
    let mut host = FakeHost::new(
        None,
        vec!["/home/u/.cargo/bin/shepr", "/home/u/.local/bin/shepr"],
        "/home/u/.local/bin/shepr",
    );
    host.sh = Some("/opt/shepr");
    // Every attempt runs out of time after two round trips.
    host.fail_at = vec![2, 5, 8];
    let mut progress = DiscoveryProgress::default();
    let mut attempts = 0;
    let found = loop {
        attempts += 1;
        assert!(attempts <= 4, "discovery must finish: {:?}", host.calls);
        match progress.advance(&mut host) {
            Ok(found) => break found,
            Err(error) => {
                assert_eq!(error.kind(), io::ErrorKind::TimedOut);
                assert!(progress.has_progress());
            }
        }
    };
    assert_eq!(found.as_str(), "/home/u/.local/bin/shepr");
    // Nothing that completed ran again; only the interrupted round trip repeats.
    assert_eq!(
        host.calls,
        [
            "login",
            "sh",
            "known",
            "known",
            "probe /opt/shepr",
            "probe /home/u/.cargo/bin/shepr",
            "probe /home/u/.cargo/bin/shepr",
            "probe /home/u/.local/bin/shepr",
        ]
    );
}

#[test]
fn a_first_round_trip_that_times_out_leaves_no_progress() {
    let mut host = FakeHost::new(Some("/usr/bin/shepr"), Vec::new(), "/usr/bin/shepr");
    host.fail_at = vec![0];
    let mut progress = DiscoveryProgress::default();
    assert!(progress.advance(&mut host).is_err());
    assert!(!progress.has_progress());
    assert_eq!(
        progress
            .advance(&mut host)
            .expect("second attempt")
            .as_str(),
        "/usr/bin/shepr"
    );
}

#[test]
fn progress_survives_only_link_failures() {
    let host = || {
        FakeHost::new(
            Some("/usr/bin/shepr"),
            vec!["/home/u/.cargo/bin/shepr"],
            "/home/u/.cargo/bin/shepr",
        )
    };

    // The second probe times out: the next attempt resumes at that probe.
    let mut timed_out = host();
    timed_out.fail_at = vec![3];
    let mut progress = DiscoveryProgress::default();
    let error = progress
        .advance(&mut timed_out)
        .expect_err("probe times out");
    assert!(is_ssh_link_failure(&error));
    assert!(progress.has_progress());
    assert!(progress.advance(&mut timed_out).is_ok());
    assert_eq!(
        timed_out.calls,
        [
            "login",
            "known",
            "probe /usr/bin/shepr",
            "probe /home/u/.cargo/bin/shepr",
            "probe /home/u/.cargo/bin/shepr",
        ]
    );

    // The same probe fails some other way: the next attempt starts over.
    let mut failed = host();
    failed.fail_other_at = vec![3];
    let mut progress = DiscoveryProgress::default();
    let error = progress.advance(&mut failed).expect_err("probe fails");
    assert!(!is_ssh_link_failure(&error));
    assert!(!progress.has_progress());
    assert!(progress.advance(&mut failed).is_ok());
    assert_eq!(
        failed.calls,
        [
            "login",
            "known",
            "probe /usr/bin/shepr",
            "probe /home/u/.cargo/bin/shepr",
            "login",
            "known",
            "probe /usr/bin/shepr",
            "probe /home/u/.cargo/bin/shepr",
        ]
    );
}

fn ssh_output(code: i32, stderr: &str) -> Output {
    use std::os::unix::process::ExitStatusExt as _;
    Output {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

#[test]
fn ssh_exit_255_from_a_discovery_command_is_a_link_failure() {
    let lost = command_failed(
        "remote SSH connection failed",
        &ssh_output(255, "Connection reset by peer"),
    );
    assert!(is_ssh_link_failure(&lost));
    assert_eq!(
        lost.to_string(),
        "remote SSH connection failed: Connection reset by peer"
    );
    let remote = command_failed("remote binary discovery failed", &ssh_output(1, "boom"));
    assert!(!is_ssh_link_failure(&remote));
    assert_eq!(remote.to_string(), "remote binary discovery failed: boom");
    // A `command -v` lookup whose ssh failed is not "no shepr on PATH".
    assert!(path_lookup_result(&ssh_output(1, "")).is_ok_and(|path| path.is_none()));
    let error = path_lookup_result(&ssh_output(255, "Connection timed out"))
        .expect_err("ssh failure is not a lookup result");
    assert!(is_ssh_link_failure(&error));

    // And discovery keeps its progress across it.
    struct LinkDrop(FakeHost);
    impl DiscoverySteps for LinkDrop {
        fn path_via_login_shell(&mut self) -> io::Result<Option<RemoteExecutable>> {
            self.0.path_via_login_shell()
        }
        fn path_via_sh(&mut self) -> io::Result<Option<RemoteExecutable>> {
            self.0.path_via_sh()
        }
        fn known_locations(&mut self) -> io::Result<Vec<RemoteExecutable>> {
            self.0.known_locations()
        }
        fn matches(&mut self, candidate: &RemoteExecutable) -> io::Result<bool> {
            if self.0.calls.len() == 2 {
                self.0.calls.push("dropped".into());
                return Err(command_failed(
                    "remote SSH connection failed",
                    &ssh_output(255, "Broken pipe"),
                ));
            }
            self.0.matches(candidate)
        }
        fn target(&self) -> &str {
            self.0.target()
        }
    }
    let mut host = LinkDrop(FakeHost::new(
        Some("/usr/bin/shepr"),
        Vec::new(),
        "/usr/bin/shepr",
    ));
    let mut progress = DiscoveryProgress::default();
    assert!(progress.advance(&mut host).is_err());
    assert!(progress.has_progress());
    assert!(progress.advance(&mut host).is_ok());
    assert_eq!(
        host.0.calls,
        ["login", "known", "dropped", "probe /usr/bin/shepr"]
    );
}

#[test]
fn command_remote_stderr_is_filtered_before_error_output() {
    let error = command_failed(
        "remote binary discovery failed",
        &ssh_output(1, "Connection refused\x1b[2J"),
    );
    assert!(!error.to_string().contains('\x1b'));
    assert!(error.to_string().contains("Connection refused?[2J"));

    let authentication = command_failed(
        "remote SSH connection failed",
        &ssh_output(255, "Permission denied (publickey)\x1b[2J"),
    );
    assert!(crate::SshFailureDiagnostic::from_error(&authentication).requires_authentication());
    assert!(!authentication.to_string().contains('\x1b'));
}

#[test]
fn exhausted_discovery_reports_not_ready_and_starts_over_next_time() {
    let mut host = FakeHost::new(Some("/usr/bin/shepr"), Vec::new(), "/nowhere");
    let mut progress = DiscoveryProgress::default();
    let error = progress
        .advance(&mut host)
        .expect_err("no candidate matches");
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert!(!progress.has_progress());
    // The install may have changed; the next attempt looks again from the start.
    host.matching = "/usr/bin/shepr";
    host.calls.clear();
    assert!(progress.advance(&mut host).is_ok());
    assert_eq!(host.calls, ["login", "known", "probe /usr/bin/shepr"]);
}

#[test]
fn remote_client_status_requires_an_exact_build_id() {
    let matching = RemoteClientStatusJson {
        version: Some("old-version".into()),
        build_id: Some(shepr_protocol::BUILD_ID.into()),
    };
    assert!(ensure_remote_client_build("build", &matching).is_ok());

    let other_build = if shepr_protocol::BUILD_ID == "ffffffffffffffff" {
        "0000000000000000"
    } else {
        "ffffffffffffffff"
    };
    let mismatched = RemoteClientStatusJson {
        version: Some(shepr_protocol::build_version()),
        build_id: Some(other_build.into()),
    };
    let error = ensure_remote_client_build("build", &mismatched).expect_err("build mismatch");
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert!(error.to_string().contains(other_build));
}

#[test]
fn client_build_mismatch_offers_a_separate_remote_session() {
    let other_build = if shepr_protocol::BUILD_ID == "ffffffffffffffff" {
        "0000000000000000"
    } else {
        "ffffffffffffffff"
    };
    let mismatched = RemoteClientStatusJson {
        version: Some(shepr_protocol::build_version()),
        build_id: Some(other_build.into()),
    };
    let error = ensure_remote_client_build("build", &mismatched).expect_err("build mismatch");
    assert!(
        error.to_string().contains("--remote-session <name>"),
        "{error}"
    );
}

#[test]
fn client_build_mismatch_filters_remote_text_with_the_shared_rule() {
    let mismatched = RemoteClientStatusJson {
        version: Some("1.0\x1b[2J".into()),
        build_id: Some("build id".into()),
    };
    let error = ensure_remote_client_build("build", &mismatched).expect_err("build mismatch");
    assert!(
        error
            .to_string()
            .contains("found version unknown build build id")
    );
    assert!(!error.to_string().contains('\x1b'));
}

#[test]
fn exhausted_discovery_names_a_path_rejected_for_shell_quoting() {
    struct QuotedInstall(Option<RejectedShellUnsafeCandidate>);
    impl DiscoverySteps for QuotedInstall {
        fn path_via_login_shell(&mut self) -> io::Result<Option<RemoteExecutable>> {
            Ok(
                remote_executable_from_path_discovery_with_rejected_candidate(
                    "/home/a b/bin/shepr\n",
                    &mut self.0,
                ),
            )
        }
        fn path_via_sh(&mut self) -> io::Result<Option<RemoteExecutable>> {
            Ok(None)
        }
        fn known_locations(&mut self) -> io::Result<Vec<RemoteExecutable>> {
            Ok(Vec::new())
        }
        fn matches(&mut self, _candidate: &RemoteExecutable) -> io::Result<bool> {
            Ok(false)
        }
        fn target(&self) -> &str {
            "build"
        }
        fn take_rejected_shell_unsafe_candidate(&mut self) -> Option<RejectedShellUnsafeCandidate> {
            self.0.take()
        }
    }

    let error = DiscoveryProgress::default()
        .advance(&mut QuotedInstall(None))
        .expect_err("the only install needs quoting");
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    let message = error.to_string();
    assert!(message.contains("\"/home/a b/bin/shepr\""), "{message}");
    assert!(message.contains("shell-safe"), "{message}");

    let mut rejected = None;
    assert!(
        remote_executable_from_path_discovery_with_rejected_candidate("bin/shepr\n", &mut rejected)
            .is_none()
    );
    assert!(
        rejected.is_none(),
        "a relative path is not a quoting rejection"
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
