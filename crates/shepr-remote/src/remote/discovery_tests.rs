//! Full discovery of a configured machine's remote executable, resumed across attempts.

use super::*;

fn executable(path: &'static str) -> RemoteExecutable {
    RemoteExecutable::parse(path).expect("test precondition")
}

/// A remote host reached through fake round trips. `fail_at` names the call (counted
/// from zero over this fake's whole life) that times out, as an attempt deadline would.
struct FakeHost {
    account_shell: Option<&'static str>,
    sh: Option<&'static str>,
    known: Vec<&'static str>,
    matching: &'static str,
    calls: Vec<String>,
    fail_at: Vec<usize>,
    /// Calls that fail with an error other than a link failure.
    fail_other_at: Vec<usize>,
    fail_authentication_wait_at: Vec<usize>,
    fail_host_key_at: Vec<usize>,
}

impl FakeHost {
    fn new(
        account_shell: Option<&'static str>,
        known: Vec<&'static str>,
        matching: &'static str,
    ) -> Self {
        Self {
            account_shell,
            sh: None,
            known,
            matching,
            calls: Vec::new(),
            fail_at: Vec::new(),
            fail_other_at: Vec::new(),
            fail_authentication_wait_at: Vec::new(),
            fail_host_key_at: Vec::new(),
        }
    }

    fn call(&mut self, name: String) -> io::Result<()> {
        let index = self.calls.len();
        self.calls.push(name);
        if self.fail_at.contains(&index) {
            return Err(attempt_deadline_passed());
        }
        if self.fail_authentication_wait_at.contains(&index) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                crate::SshFailureDiagnostic::authentication_wait_timeout(),
            ));
        }
        if self.fail_host_key_at.contains(&index) {
            return Err(io::Error::other(
                crate::SshFailureDiagnostic::from_ssh_output(
                    Some(crate::SSH_OWN_FAILURE_EXIT_CODE),
                    "Host key verification failed".into(),
                ),
            ));
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
    fn path_via_account_shell(&mut self) -> io::Result<Option<RemoteExecutable>> {
        self.call("account-shell".into())?;
        Ok(self.account_shell.map(executable))
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
    // The account shell found a path, so the /bin/sh fallback is skipped, and the
    // duplicate from the known locations is probed once.
    assert_eq!(
        host.calls,
        [
            "account-shell",
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
            "account-shell",
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
fn progress_survives_link_and_authentication_wait_timeouts_but_not_ssh_failures() {
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
            "account-shell",
            "known",
            "probe /usr/bin/shepr",
            "probe /home/u/.cargo/bin/shepr",
            "probe /home/u/.cargo/bin/shepr",
        ]
    );

    // A full round-trip timeout may be waiting for interactive authentication.
    // It has no remote result, so completed discovery steps remain useful.
    let mut authentication_wait = host();
    authentication_wait.fail_authentication_wait_at = vec![3];
    let mut progress = DiscoveryProgress::default();
    let error = progress
        .advance(&mut authentication_wait)
        .expect_err("authentication wait times out");
    assert!(crate::SshFailureDiagnostic::from_error(&error).is_authentication_wait_timeout());
    assert!(progress.has_progress());
    assert!(progress.advance(&mut authentication_wait).is_ok());
    assert_eq!(
        authentication_wait.calls,
        [
            "account-shell",
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
            "account-shell",
            "known",
            "probe /usr/bin/shepr",
            "probe /home/u/.cargo/bin/shepr",
            "account-shell",
            "known",
            "probe /usr/bin/shepr",
            "probe /home/u/.cargo/bin/shepr",
        ]
    );

    // An SSH failure is different: a changed host key means prior candidate
    // data may describe another machine, so the next attempt starts over.
    let mut host_key_changed = host();
    host_key_changed.fail_host_key_at = vec![3];
    let mut progress = DiscoveryProgress::default();
    let error = progress
        .advance(&mut host_key_changed)
        .expect_err("host-key rejection");
    assert!(crate::SshFailureDiagnostic::from_error(&error).is_host_key());
    assert!(!progress.has_progress());
    assert!(progress.advance(&mut host_key_changed).is_ok());
    assert_eq!(
        host_key_changed.calls,
        [
            "account-shell",
            "known",
            "probe /usr/bin/shepr",
            "probe /home/u/.cargo/bin/shepr",
            "account-shell",
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
        fn path_via_account_shell(&mut self) -> io::Result<Option<RemoteExecutable>> {
            self.0.path_via_account_shell()
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
        ["account-shell", "known", "dropped", "probe /usr/bin/shepr"]
    );
}

#[test]
fn status_probe_diagnostic_distinguishes_ssh_failure_from_remote_failure() {
    let ssh_failure = remote_client_status_failure(&ssh_output(255, "Connection refused"));
    assert_eq!(
        ssh_failure.to_string(),
        "remote SSH connection failed: Connection refused"
    );

    let remote_failure = remote_client_status_failure(&ssh_output(2, "status command failed"));
    assert_eq!(
        remote_failure.to_string(),
        "remote client status probe failed: status command failed"
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
    assert_eq!(
        host.calls,
        ["account-shell", "known", "probe /usr/bin/shepr"]
    );
}

#[test]
fn remote_client_status_requires_an_exact_build_id() {
    let matching = shepr_api::schema::ClientStatusJson {
        version: Some("old-version".into()),
        build_id: Some(shepr_protocol::BUILD_ID.into()),
        binary: None,
        server: None,
    };
    assert!(ensure_remote_client_build("build", &matching).is_ok());

    let other_build = if shepr_protocol::BUILD_ID == "ffffffffffffffff" {
        "0000000000000000"
    } else {
        "ffffffffffffffff"
    };
    let mismatched = shepr_api::schema::ClientStatusJson {
        version: Some(shepr_protocol::build_version()),
        build_id: Some(other_build.into()),
        binary: None,
        server: None,
    };
    let error = ensure_remote_client_build("build", &mismatched).expect_err("build mismatch");
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert!(error.to_string().contains(other_build));
}

#[test]
fn client_build_mismatch_filters_remote_text_with_the_shared_rule() {
    let mismatched = shepr_api::schema::ClientStatusJson {
        version: Some("1.0\x1b[2J".into()),
        build_id: Some("build id".into()),
        binary: None,
        server: None,
    };
    let error = ensure_remote_client_build("build", &mismatched).expect_err("build mismatch");
    assert!(
        error
            .to_string()
            .contains("found version unknown build build id")
    );
    assert!(!error.to_string().contains('\x1b'));
}

fn client_status_with_sibling(
    server: Option<shepr_api::schema::SiblingServerJson>,
) -> shepr_api::schema::ClientStatusJson {
    shepr_api::schema::ClientStatusJson {
        version: Some(shepr_protocol::build_version()),
        build_id: Some(shepr_protocol::BUILD_ID.into()),
        binary: None,
        server,
    }
}

fn sibling(build_id: Option<&str>, error: Option<&str>) -> shepr_api::schema::SiblingServerJson {
    shepr_api::schema::SiblingServerJson {
        binary: Some("/home/u/.cargo/bin/shepr-server".into()),
        version: build_id.map(|_| "1.0".into()),
        build_id: build_id.map(str::to_owned),
        error: error.map(str::to_owned),
    }
}

#[test]
fn the_remote_pair_check_requires_a_sibling_of_this_build() {
    let matching = client_status_with_sibling(Some(sibling(Some(shepr_protocol::BUILD_ID), None)));
    assert!(ensure_remote_sibling_build("host", &matching).is_ok());

    let other_build = if shepr_protocol::BUILD_ID == "ffffffffffffffff" {
        "0000000000000000"
    } else {
        "ffffffffffffffff"
    };
    let stale = client_status_with_sibling(Some(sibling(Some(other_build), None)));
    let error = ensure_remote_sibling_build("host", &stale).expect_err("stale sibling");
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert!(error.to_string().contains(other_build), "{error}");
    assert!(error.to_string().contains("shepr-server"), "{error}");
}

#[test]
fn a_missing_or_unreported_remote_sibling_is_an_install_error() {
    let unreported = client_status_with_sibling(None);
    let error = ensure_remote_sibling_build("host", &unreported).expect_err("no report");
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert!(error.to_string().contains("did not report"), "{error}");

    let missing = client_status_with_sibling(Some(sibling(
        None,
        Some("shepr-server was not found at /home/u/.cargo/bin/shepr-server"),
    )));
    let error = ensure_remote_sibling_build("host", &missing).expect_err("missing sibling");
    assert!(error.to_string().contains("was not found"), "{error}");
    assert!(
        error
            .to_string()
            .contains("/home/u/.cargo/bin/shepr-server"),
        "{error}"
    );
}

#[test]
fn remote_sibling_text_is_filtered_before_local_output() {
    let hostile = client_status_with_sibling(Some(shepr_api::schema::SiblingServerJson {
        binary: Some("/bin/\x1b[2Jshepr-server".into()),
        version: Some("1.0\x1b[2J".into()),
        build_id: Some("\x1b[2J".into()),
        error: Some("boom\x1b[2J".into()),
    }));
    let error = ensure_remote_sibling_build("host", &hostile).expect_err("hostile report");
    assert!(!error.to_string().contains('\x1b'), "{error}");

    let hostile = client_status_with_sibling(Some(shepr_api::schema::SiblingServerJson {
        binary: None,
        version: Some("1.0\x1b[2J".into()),
        build_id: Some("\x1b[2J".into()),
        error: None,
    }));
    let error = ensure_remote_sibling_build("host", &hostile).expect_err("hostile report");
    assert!(!error.to_string().contains('\x1b'), "{error}");
}

#[test]
fn exhausted_discovery_names_a_path_rejected_for_shell_quoting() {
    struct QuotedInstall(Option<RejectedShellUnsafeCandidate>);
    impl DiscoverySteps for QuotedInstall {
        fn path_via_account_shell(&mut self) -> io::Result<Option<RemoteExecutable>> {
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
        remote_shepr.bridge_command(),
        format!(
            "/bin/sh -c 'echo; echo shepr-remote-output-ready; /usr/bin/shepr remote-client-bridge; shepr_exit_status=$?; if [ $shepr_exit_status -eq {SSH_OWN_FAILURE_EXIT_CODE} ]; then exit {REMAPPED_REMOTE_255_EXIT_CODE}; fi; exit $shepr_exit_status'"
        )
    );
}

#[test]
fn remote_path_discovery_ignores_binaries_that_need_quoting() {
    // The bridge script must reach the account shell as one quoted word
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
fn remote_path_discovery_keeps_mise_shims_for_build_probe() {
    let candidates = remote_executables_from_path_discovery(
        "/home/can/.local/share/mise/shims/shepr\n/home/can/.local/share/mise/installs/shepr/0.7.1/bin/shepr\n",
    );

    assert_eq!(candidates.len(), 2);
    assert_eq!(
        candidates[0].as_str(),
        "/home/can/.local/share/mise/shims/shepr"
    );
    assert_eq!(
        candidates[1].as_str(),
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
fn known_remote_binary_candidate_script_includes_cargo_home_and_local_bin() {
    let script = known_remote_binary_candidate_script();

    assert!(script.contains("cargo_home=${CARGO_HOME:-}"));
    assert!(script.contains("cargo_home=\"$home/.cargo\""));
    assert!(script.contains(&format!("emit \"$cargo_home/bin/{REMOTE_INSTALL_NAME}\"")));
    assert!(script.contains(&format!("emit \"$home/.local/bin/{REMOTE_INSTALL_NAME}\"")));
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

struct RejectingHost {
    probes: Vec<String>,
    matches: &'static str,
    /// The first candidate's probe fails outright instead of reporting a
    /// different build.
    probe_fails: bool,
}

impl DiscoverySteps for RejectingHost {
    fn path_via_account_shell(&mut self) -> io::Result<Option<RemoteExecutable>> {
        Ok(Some(executable("/usr/bin/shepr")))
    }

    fn path_via_sh(&mut self) -> io::Result<Option<RemoteExecutable>> {
        Ok(None)
    }

    fn known_locations(&mut self) -> io::Result<Vec<RemoteExecutable>> {
        Ok(vec![executable("/home/user/.cargo/bin/shepr")])
    }

    fn matches(&mut self, candidate: &RemoteExecutable) -> io::Result<bool> {
        self.probes.push(candidate.as_str().to_owned());
        if candidate.as_str() == "/usr/bin/shepr" {
            if self.probe_fails {
                return Err(io::Error::other(
                    "remote client status probe failed (exit status 2)",
                ));
            }
            return Err(remote_candidate_mismatch(
                "remote Shepr compatibility error: different build".into(),
            ));
        }
        Ok(candidate.as_str() == self.matches)
    }

    fn target(&self) -> &str {
        "build"
    }
}

#[test]
fn an_incompatible_candidate_does_not_hide_a_later_match() {
    let mut host = RejectingHost {
        probes: Vec::new(),
        matches: "/home/user/.cargo/bin/shepr",
        probe_fails: false,
    };
    let found = DiscoveryProgress::default()
        .advance(&mut host)
        .expect("the later compatible candidate should be found");
    assert_eq!(found.as_str(), "/home/user/.cargo/bin/shepr");
    assert_eq!(
        host.probes,
        vec![
            "/usr/bin/shepr".to_owned(),
            "/home/user/.cargo/bin/shepr".to_owned()
        ]
    );
}

#[test]
fn a_failing_status_probe_does_not_hide_a_later_match_or_its_diagnostic() {
    let mut host = RejectingHost {
        probes: Vec::new(),
        matches: "/home/user/.cargo/bin/shepr",
        probe_fails: true,
    };
    let found = DiscoveryProgress::default()
        .advance(&mut host)
        .expect("the later candidate should still be probed");
    assert_eq!(found.as_str(), "/home/user/.cargo/bin/shepr");

    let mut host = RejectingHost {
        probes: Vec::new(),
        matches: "/another/path/shepr",
        probe_fails: true,
    };
    let error = DiscoveryProgress::default()
        .advance(&mut host)
        .expect_err("no candidate matches");
    assert!(error.to_string().contains("status probe failed"), "{error}");
}

#[test]
fn the_first_candidate_mismatch_is_returned_when_none_match() {
    let mut host = RejectingHost {
        probes: Vec::new(),
        matches: "/another/path/shepr",
        probe_fails: false,
    };
    let error = DiscoveryProgress::default()
        .advance(&mut host)
        .expect_err("the incompatible candidate explains why discovery failed");
    assert!(error.to_string().contains("different build"));
    assert!(!error.to_string().contains("matching Shepr is not ready"));
    assert_eq!(host.probes.len(), 2);
}

#[test]
fn candidate_mismatch_class_excludes_other_remote_compatibility_errors() {
    let mismatch = remote_candidate_mismatch("remote candidate has a different build".into());
    let mismatch_diagnostic = crate::SshFailureDiagnostic::from_error(&mismatch);
    assert!(is_remote_candidate_mismatch(&mismatch));
    assert!(mismatch_diagnostic.is_remote_compatibility());

    for message in [
        "matching Shepr is not ready on build",
        "remote Shepr server compatibility error on build",
    ] {
        let error = crate::remote_compatibility_error(message);
        let diagnostic = crate::SshFailureDiagnostic::from_error(&error);
        assert!(diagnostic.is_remote_compatibility(), "{message}");
        assert!(!is_remote_candidate_mismatch(&error), "{message}");
    }
}
