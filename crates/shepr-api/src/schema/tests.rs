use std::collections::HashSet;

use super::*;

#[test]
fn method_names_are_unique() {
    let mut names = HashSet::new();
    for name in Method::ALL_NAMES {
        assert!(names.insert(*name), "duplicate method name: {name}");
    }
}

#[test]
fn request_uses_dot_method_names() {
    let request = Request {
        id: "req_1".into(),
        method: Method::PaneReportAgentSession(PaneReportAgentSessionParams {
            pane_id: "w1:p1".into(),
            source: "shepr:pi".into(),
            agent: "pi".into(),
            seq: None,
            agent_session_id: None,
            agent_session_path: None,
            session_start_source: None,
        }),
    };

    let json = serde_json::to_value(&request).expect("test precondition");
    assert_eq!(json["method"], "pane.report_agent_session");
}

#[test]
fn request_round_trips_for_server_stop() {
    let request = Request {
        id: "req_stop".into(),
        method: Method::ServerStop(ServerStopParams::default()),
    };

    let json = serde_json::to_value(&request).expect("test precondition");
    assert_eq!(json["method"], "server.stop");
    let restored: Request = serde_json::from_value(json).expect("test precondition");
    assert_eq!(restored, request);
}

#[test]
fn server_stop_without_params_is_unconditional() {
    let request: Request = serde_json::from_str(r#"{"id":"s","method":"server.stop","params":{}}"#)
        .expect("test precondition");
    assert_eq!(
        request.method,
        Method::ServerStop(ServerStopParams::default())
    );
}

#[test]
fn server_stop_rejects_a_boot_guard_on_the_unconditional_method() {
    let request = r#"{"id":"s","method":"server.stop","params":{"expected_boot_id":"17-23"}}"#;
    assert!(serde_json::from_str::<Request>(request).is_err());

    let missing_boot = r#"{"id":"s","method":"server.stop_if_boot","params":{}}"#;
    assert!(serde_json::from_str::<Request>(missing_boot).is_err());
}

#[test]
fn cross_build_ping_and_conditional_stop_json_is_frozen() {
    // Preflight can inspect and restart a server from another build. Keep the
    // ping identity and guarded stop request bytes stable across those builds.
    const PING_REQUEST: &str = r#"{"id":"cross-build:ping","method":"ping","params":{}}"#;
    const PONG_RESPONSE: &str = r#"{"id":"cross-build:ping","result":{"type":"pong","version":"0.1.2","build_id":"0123456789abcdef","boot_id":"17-23"}}"#;
    const STOP_REQUEST: &str = r#"{"id":"cross-build:stop","method":"server.stop_if_boot","params":{"expected_boot_id":"17-23"}}"#;
    const STOP_RESPONSE: &str = r#"{"id":"cross-build:stop","result":{"type":"ok"}}"#;

    let ping_request = Request {
        id: "cross-build:ping".into(),
        method: Method::Ping(PingParams::default()),
    };
    assert_eq!(
        serde_json::to_string(&ping_request).expect("test precondition"),
        PING_REQUEST
    );
    assert_eq!(
        serde_json::from_str::<Request>(PING_REQUEST).expect("test precondition"),
        ping_request
    );

    let pong_response = SuccessResponse {
        id: "cross-build:ping".into(),
        result: ResponseResult::Pong {
            version: "0.1.2".into(),
            build_id: "0123456789abcdef".into(),
            boot_id: "17-23".into(),
        },
    };
    assert_eq!(
        serde_json::to_string(&pong_response).expect("test precondition"),
        PONG_RESPONSE
    );
    assert_eq!(
        serde_json::from_str::<SuccessResponse>(PONG_RESPONSE).expect("test precondition"),
        pong_response
    );

    let stop_request = Request {
        id: "cross-build:stop".into(),
        method: Method::ServerStopIfBoot(ServerStopIfBootParams {
            expected_boot_id: "17-23".into(),
        }),
    };
    assert_eq!(
        serde_json::to_string(&stop_request).expect("test precondition"),
        STOP_REQUEST
    );
    assert_eq!(
        serde_json::from_str::<Request>(STOP_REQUEST).expect("test precondition"),
        stop_request
    );

    let stop_response = SuccessResponse {
        id: "cross-build:stop".into(),
        result: ResponseResult::Ok {},
    };
    assert_eq!(
        serde_json::to_string(&stop_response).expect("test precondition"),
        STOP_RESPONSE
    );
    assert_eq!(
        serde_json::from_str::<SuccessResponse>(STOP_RESPONSE).expect("test precondition"),
        stop_response
    );
}

#[test]
fn detect_requests_take_a_pane_id_and_round_trip() {
    for (name, method) in [
        (
            "detect.capture",
            Method::DetectCapture(PaneTarget {
                pane_id: "w1:p1".into(),
            }),
        ),
        (
            "detect.explain",
            Method::DetectExplain(PaneTarget {
                pane_id: "w1:p1".into(),
            }),
        ),
    ] {
        let request = Request {
            id: "req_detect".into(),
            method,
        };

        let json = serde_json::to_value(&request).expect("test precondition");
        assert_eq!(json["method"], name);
        assert_eq!(json["params"]["pane_id"], "w1:p1");
        let restored: Request = serde_json::from_value(json).expect("test precondition");
        assert_eq!(restored, request);
    }
}

#[test]
fn unknown_method_is_rejected() {
    let json = r#"{"id":"req_1","method":"nope","params":{}}"#;
    let err = serde_json::from_str::<Request>(json)
        .expect_err("test precondition")
        .to_string();
    assert!(err.contains("unknown variant"));
}

#[test]
fn removed_methods_are_rejected() {
    for method in [
        "pane.send_text",
        "pane.send_keys",
        "pane.send_input",
        "pane.wait_for_output",
        "server.agent_manifests",
        "server.reload_agent_manifests",
        "agent.read",
        "agent.explain",
        "agent.list",
        "agent.get",
        "agent.rename",
        "agent.focus",
        "workspace.list",
        "workspace.get",
        "tab.list",
        "tab.get",
        "pane.list",
        "pane.current",
        "pane.read",
        "pane.layout",
        "pane.process_info",
        "pane.neighbor",
        "pane.edges",
        "pane.move",
        "client.window_title.set",
        "client.window_title.clear",
    ] {
        let request = serde_json::json!({"id": "req", "method": method, "params": {}});
        let error = serde_json::from_value::<Request>(request).expect_err("removed method");
        assert!(
            error.to_string().contains("unknown variant"),
            "{method}: {error}"
        );
    }
}

#[test]
fn removed_uncalled_methods_are_rejected() {
    for method in [
        "layout.export",
        "layout.apply",
        "workspace.move_block",
        "pane.clear_agent_authority",
        "pane.get",
        "events.subscribe",
        "events.wait",
        "session.snapshot",
        "server.ssh_agent.register",
    ] {
        let request = serde_json::json!({"id": "req", "method": method, "params": {}});
        let error = serde_json::from_value::<Request>(request).expect_err("removed method");
        assert!(
            error.to_string().contains("unknown variant"),
            "{method}: {error}"
        );
    }
}

/// A client shell's commands cross the client socket as
/// `shepr_protocol::command::EndpointCommand`; none of them is a JSON API
/// method.
#[test]
fn client_shell_commands_are_not_api_methods() {
    for method in [
        "client_shell.surface.set",
        "workspace.create",
        "workspace.focus",
        "workspace.rename",
        "workspace.checkout_root",
        "workspace.move",
        "workspace.close",
        "pane.split",
        "pane.swap",
        "pane.zoom",
        "layout.set_split_ratio",
        "pane.focus_direction",
        "pane.resize",
        "pane.scroll",
        "pane.clear",
        "pane.selection.read",
        "pane.copy_motion",
        "pane.copy_search",
        "pane.focus",
        "pane.input.set",
        "pane.rename",
        "pane.close",
    ] {
        let request = serde_json::json!({"id": "req", "method": method, "params": {}});
        let error = serde_json::from_value::<Request>(request).expect_err("client-shell command");
        assert!(
            error.to_string().contains("unknown variant"),
            "{method}: {error}"
        );
    }
}

#[test]
fn agent_status_accepts_only_presentable_states() {
    for status in ["idle", "working", "blocked"] {
        assert!(serde_json::from_str::<AgentStatus>(&format!("\"{status}\"")).is_ok());
    }
    for status in ["done", "unknown", "future_status"] {
        assert!(serde_json::from_str::<AgentStatus>(&format!("\"{status}\"")).is_err());
    }
}

#[test]
fn success_response_round_trips() {
    let response = SuccessResponse {
        id: "req_1".into(),
        result: ResponseResult::Pong {
            version: "0.1.2".into(),
            build_id: "0123456789abcdef".into(),
            boot_id: "17-23".into(),
        },
    };

    let json = serde_json::to_string(&response).expect("test precondition");
    let restored: SuccessResponse = serde_json::from_str(&json).expect("test precondition");
    assert_eq!(restored, response);
}

#[test]
fn error_response_round_trips() {
    let response = ErrorResponse {
        id: "req_1".into(),
        error: ErrorBody {
            code: "pane_not_found".into(),
            message: "pane p_1 not found".into(),
        },
    };

    let json = serde_json::to_string(&response).expect("test precondition");
    let restored: ErrorResponse = serde_json::from_str(&json).expect("test precondition");
    assert_eq!(restored, response);
}
