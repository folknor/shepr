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
