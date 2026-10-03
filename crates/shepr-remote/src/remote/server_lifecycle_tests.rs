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
        version: Some("0.6.0".into()),
        build_id: Some("0123456789abcdef".parse().expect("build identity")),
        boot_id: Some("4242-1700000000".parse().expect("boot identity")),
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
}
