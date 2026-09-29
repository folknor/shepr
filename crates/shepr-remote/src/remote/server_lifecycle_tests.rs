use super::*;

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
            read_remote_confirmation(&mut "YES\n".as_bytes(), default).expect("test precondition")
        );
        assert!(
            !read_remote_confirmation(&mut "no\n".as_bytes(), default).expect("test precondition")
        );
    }
}

#[test]
fn parse_remote_server_status_json_reads_running_server() {
    assert_eq!(
        parse_remote_server_status_json(
            r#"{"running":true,"version":"0.6.0","build_id":"0123456789abcdef","capabilities":{"detached_server_daemon":true,"ssh_agent_registration":false},"compatible":true,"socket":"/run/shepr.sock","session":null,"restart_needed":false}"#
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
            r#"{"running":false,"version":null,"build_id":null,"capabilities":null,"compatible":null,"socket":"/run/shepr.sock","session":null,"restart_needed":false}"#
        )
        .expect("test precondition"),
        RemoteServerStatus::NotRunning
    );
}
