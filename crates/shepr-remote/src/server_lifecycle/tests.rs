use super::*;

fn status_json(presence: &str, identity: bool) -> String {
    let identity = if identity {
        r#""version":"0.6.0","build_id":"0123456789abcdef","boot_id":"4242-1700000000""#
    } else {
        r#""version":null,"build_id":null,"boot_id":null"#
    };
    format!(r#"{{"presence":"{presence}",{identity},"socket":"/run/shepr.sock"}}"#)
}

fn answered() -> RemoteServerStatus {
    RemoteServerStatus::Running {
        build_id: "0123456789abcdef".parse().expect("build identity"),
        boot_id: "4242-1700000000".parse().expect("boot identity"),
    }
}

#[test]
fn parse_remote_server_status_json_reads_running_server() {
    assert_eq!(
        parse_remote_server_status_json(&status_json("running", true)).expect("test precondition"),
        answered()
    );
}

#[test]
fn a_starting_remote_server_is_judged_by_its_identity() {
    assert_eq!(
        parse_remote_server_status_json(&status_json("starting", true)).expect("test precondition"),
        answered()
    );
}

#[test]
fn parse_remote_server_status_json_reads_stopped_server() {
    assert_eq!(
        parse_remote_server_status_json(&status_json("gone", false)).expect("test precondition"),
        RemoteServerStatus::NotRunning
    );
}

#[test]
fn a_stopping_remote_server_counts_as_none() {
    assert_eq!(
        parse_remote_server_status_json(&status_json("stopping", true)).expect("test precondition"),
        RemoteServerStatus::NotRunning
    );
}

#[test]
fn an_unresponsive_remote_server_fails_the_check() {
    let error = parse_remote_server_status_json(&status_json("unresponsive", false))
        .expect_err("a hung server is not ready");
    assert!(
        error.to_string().contains("not answering status requests"),
        "{error}"
    );
    assert_eq!(
        EndpointFailure::from_error(&error).cause(),
        shepr_launch::FailureCause::RemoteRepair
    );
}

fn running(build_id: &str, boot_id: &str) -> RemoteServerStatus {
    RemoteServerStatus::Running {
        build_id: build_id.parse().expect("build identity"),
        boot_id: boot_id.parse().expect("boot identity"),
    }
}

fn other_build() -> &'static str {
    if shepr_protocol::BUILD_ID == "ffffffffffffffff" {
        "0000000000000000"
    } else {
        "ffffffffffffffff"
    }
}

/// A Restart stops only a server of another build, and only the boot its
/// status named.
#[test]
fn a_restart_stops_the_observed_boot_of_another_build_only() {
    for status in [
        RemoteServerStatus::NotRunning,
        running(shepr_protocol::BUILD_ID, "17-23"),
    ] {
        stop_for_restart(status, |_| panic!("nothing to stop")).expect("no stop needed");
    }

    let mut stopped = Vec::new();
    stop_for_restart(running(other_build(), "17-23"), |boot| {
        stopped.push(boot.to_string());
        Ok(StopOutcome::Stopped)
    })
    .expect("the observed server stopped");
    assert_eq!(stopped, ["17-23"]);

    stop_for_restart(running(other_build(), "17-23"), |_| {
        Ok(StopOutcome::NoServer)
    })
    .expect("a server already gone needs no stop");

    let error = stop_for_restart(running(other_build(), "17-23"), |_| {
        Ok(StopOutcome::BootChanged)
    })
    .expect_err("a replaced server is left running");
    assert!(error.to_string().contains("left running"), "{error}");
}

#[test]
fn invalid_remote_status_json_error_does_not_echo_control_bytes() {
    let injected = "\x1b[2J";
    let parse_error = parse_remote_server_status_json(injected).expect_err("invalid JSON");
    assert!(!parse_error.to_string().contains('\x1b'));
}

#[test]
fn shared_remote_text_filter_keeps_printable_lines_and_rejects_controls() {
    assert_eq!(
        RemoteText::from_untrusted("Connection refused\n\x1b[2J").to_string(),
        "Connection refused\n?[2J"
    );
    assert_eq!(
        RemoteText::from_untrusted("Warning: added host\r\nbanner \u{9b}2J caf\u{e9}\ttab")
            .to_string(),
        "Warning: added host\nbanner ?2J caf\u{e9}\ttab"
    );
    assert_eq!(
        remote_display_value(Some("build id")).to_string(),
        "build id"
    );
    assert_eq!(
        remote_display_value(Some("bad\tvalue")).to_string(),
        "unknown"
    );
}

#[test]
fn server_status_skips_shell_noise_before_and_after_the_record() {
    let status = status_json("running", true);
    assert_eq!(
        parse_remote_server_status_json(&format!("banner\n{status}\ntrailer\n"))
            .expect("status record"),
        parse_remote_server_status_json(&status).expect("plain status"),
    );
}
