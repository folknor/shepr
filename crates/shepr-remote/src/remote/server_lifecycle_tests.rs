use super::*;

#[test]
fn parse_remote_server_status_json_reads_running_server() {
    assert_eq!(
        parse_remote_server_status_json(
            r#"{"running":true,"version":"0.6.0","build_id":"0123456789abcdef","capabilities":{"ssh_agent_registration":false},"compatible":true,"socket":"/run/shepr.sock","restart_needed":false}"#
        )
        .expect("test precondition"),
        RemoteServerStatus::Running {
            version: Some("0.6.0".into()),
            build_id: Some("0123456789abcdef".into()),
        }
    );
}

#[test]
fn parse_remote_server_status_json_reads_stopped_server() {
    assert_eq!(
        parse_remote_server_status_json(
            r#"{"running":false,"version":null,"build_id":null,"capabilities":null,"compatible":null,"socket":"/run/shepr.sock","restart_needed":false}"#
        )
        .expect("test precondition"),
        RemoteServerStatus::NotRunning
    );
}
