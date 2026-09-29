use std::collections::{HashMap, HashSet};

use super::*;

#[test]
fn method_names_and_traits_share_unique_schema_entries() {
    let mut names = HashSet::new();
    let mut client_shell_methods = HashSet::new();

    for name in Method::ALL_NAMES {
        assert!(names.insert(*name), "duplicate method name: {name}");
        let traits = Method::traits_for_name(name).expect("declared method name");
        assert_eq!(traits.name, *name);
        if traits.client_shell {
            client_shell_methods.insert(*name);
        }
    }

    assert_eq!(
        client_shell_methods,
        HashSet::from([
            "client_shell.surface.set",
            "workspace.create",
            "workspace.focus",
            "workspace.rename",
            "workspace.move",
            "workspace.move_block",
            "workspace.close",
            "tab.create",
            "tab.focus",
            "tab.rename",
            "tab.move",
            "tab.close",
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
        ])
    );
    assert!(Method::traits_for_name("plugin.future").is_none());
}

#[test]
fn request_uses_dot_method_names() {
    let request = Request {
        id: "req_1".into(),
        method: Method::WorkspaceCreate(WorkspaceCreateParams {
            source_workspace_id: None,
            cwd: Some("/tmp".into()),
            focus: true,
            label: Some("api".into()),
            env: Default::default(),
        }),
    };

    let json = serde_json::to_value(&request).expect("test precondition");
    assert_eq!(json["method"], "workspace.create");
}

#[test]
fn request_round_trips_for_server_stop() {
    let request = Request {
        id: "req_stop".into(),
        method: Method::ServerStop(EmptyParams::default()),
    };

    let json = serde_json::to_value(&request).expect("test precondition");
    assert_eq!(json["method"], "server.stop");
    let restored: Request = serde_json::from_value(json).expect("test precondition");
    assert_eq!(restored, request);
}

#[test]
fn request_round_trips_for_server_reload_agent_manifests() {
    let request = Request {
        id: "req_reload_agent_manifests".into(),
        method: Method::ServerReloadAgentManifests(EmptyParams::default()),
    };

    let json = serde_json::to_value(&request).expect("test precondition");
    assert_eq!(json["method"], "server.reload_agent_manifests");
    let restored: Request = serde_json::from_value(json).expect("test precondition");
    assert_eq!(restored, request);
}

#[test]
fn request_round_trips_for_server_agent_manifests() {
    let request = Request {
        id: "req_agent_manifests".into(),
        method: Method::ServerAgentManifests(EmptyParams::default()),
    };

    let json = serde_json::to_value(&request).expect("test precondition");
    assert_eq!(json["method"], "server.agent_manifests");
    let restored: Request = serde_json::from_value(json).expect("test precondition");
    assert_eq!(restored, request);
}

#[test]
fn request_round_trips_for_agent_explain() {
    let request = Request {
        id: "req_agent_explain".into(),
        method: Method::AgentExplain(AgentTarget {
            target: "agent-1".into(),
        }),
    };

    let json = serde_json::to_value(&request).expect("test precondition");
    assert_eq!(json["method"], "agent.explain");
    let restored: Request = serde_json::from_value(json).expect("test precondition");
    assert_eq!(restored, request);
}

#[test]
fn client_window_title_requests_round_trip() {
    let set = Request {
        id: "req_title_set".into(),
        method: Method::ClientWindowTitleSet(ClientWindowTitleSetParams {
            title: "shepr api".into(),
        }),
    };
    let json = serde_json::to_value(&set).expect("test precondition");
    assert_eq!(json["method"], "client.window_title.set");
    assert_eq!(json["params"]["title"], "shepr api");
    let restored: Request = serde_json::from_value(json).expect("test precondition");
    assert_eq!(restored, set);

    let clear = Request {
        id: "req_title_clear".into(),
        method: Method::ClientWindowTitleClear(EmptyParams::default()),
    };
    let json = serde_json::to_value(&clear).expect("test precondition");
    assert_eq!(json["method"], "client.window_title.clear");
    let restored: Request = serde_json::from_value(json).expect("test precondition");
    assert_eq!(restored, clear);
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
fn missing_required_params_are_rejected() {
    let json = r#"{"id":"req_1","method":"pane.send_text","params":{"pane_id":"p_1"}}"#;
    let err = serde_json::from_str::<Request>(json)
        .expect_err("test precondition")
        .to_string();
    assert!(err.contains("text"));
}

#[test]
fn pane_send_input_defaults_to_empty_text_and_keys() {
    let json = r#"
    {
        "id": "req_1",
        "method": "pane.send_input",
        "params": {
            "pane_id": "p_1"
        }
    }
    "#;

    let request: Request = serde_json::from_str(json).expect("test precondition");
    let Method::PaneSendInput(params) = request.method else {
        panic!("wrong method parsed");
    };
    assert_eq!(params.pane_id, "p_1");
    assert!(params.text.is_empty());
    assert!(params.keys.is_empty());
}

#[test]
fn pane_wait_for_output_defaults_strip_ansi_to_true() {
    let json = r#"
    {
        "id": "req_1",
        "method": "pane.wait_for_output",
        "params": {
            "pane_id": "p_1",
            "source": "recent",
            "match": { "type": "substring", "value": "ready" }
        }
    }
    "#;

    let request: Request = serde_json::from_str(json).expect("test precondition");
    let Method::PaneWaitForOutput(params) = request.method else {
        panic!("wrong method parsed");
    };
    assert!(params.strip_ansi);
}

#[test]
fn pane_read_defaults_to_text_format() {
    let json = r#"
    {
        "id": "req_1",
        "method": "pane.read",
        "params": {
            "pane_id": "p_1",
            "source": "visible"
        }
    }
    "#;

    let request: Request = serde_json::from_str(json).expect("test precondition");
    let serialized = serde_json::to_value(&request).expect("test precondition");
    assert!(serialized["params"].get("intent").is_none());
    let Method::PaneRead(params) = request.method else {
        panic!("wrong method parsed");
    };
    assert_eq!(params.format, ReadFormat::Text);
    assert_eq!(params.intent, ReadIntent::Interactive);
}

#[test]
fn pane_current_request_round_trips() {
    let request = Request {
        id: "req_current".into(),
        method: Method::PaneCurrent(PaneCurrentParams {
            caller_pane_id: Some("w1-1".into()),
        }),
    };

    let json = serde_json::to_value(&request).expect("test precondition");
    assert_eq!(json["method"], "pane.current");
    assert_eq!(json["params"]["caller_pane_id"], "w1-1");
    let restored: Request = serde_json::from_value(json).expect("test precondition");
    assert_eq!(restored, request);
}

#[test]
fn pane_process_info_request_round_trips() {
    let request = Request {
        id: "req_process_info".into(),
        method: Method::PaneProcessInfo(PaneProcessInfoParams {
            pane_id: Some("w1-1".into()),
        }),
    };

    let json = serde_json::to_value(&request).expect("test precondition");
    assert_eq!(json["method"], "pane.process_info");
    assert_eq!(json["params"]["pane_id"], "w1-1");
    let restored: Request = serde_json::from_value(json).expect("test precondition");
    assert_eq!(restored, request);
}

#[test]
fn event_envelope_round_trips() {
    let events = [
        EventEnvelope {
            data: EventData::PaneExited {
                pane_id: "p_1".into(),
                workspace_id: "w_1".into(),
            },
        },
        EventEnvelope {
            data: EventData::WorkspaceMoved {
                workspace_id: "w_1".into(),
                insert_index: 2,
                workspaces: vec![],
            },
        },
        EventEnvelope {
            data: EventData::WorkspaceReordered {
                workspace_ids: vec!["w_1".into(), "w_2".into()],
                before_workspace_id: Some("w_3".into()),
                workspaces: vec![],
            },
        },
        EventEnvelope {
            data: EventData::TabMoved {
                tab_id: "w_1:1".into(),
                workspace_id: "w_1".into(),
                insert_index: 1,
                tabs: vec![],
            },
        },
        EventEnvelope {
            data: EventData::LayoutUpdated {
                layout: PaneLayoutSnapshot {
                    workspace_id: "w_1".into(),
                    tab_id: "w_1:1".into(),
                    zoomed: false,
                    area: PaneLayoutRect {
                        x: 0,
                        y: 0,
                        width: 100,
                        height: 24,
                    },
                    focused_pane_id: "w_1-1".into(),
                    panes: vec![PaneLayoutPane {
                        pane_id: "w_1-1".into(),
                        focused: true,
                        rect: PaneLayoutRect {
                            x: 0,
                            y: 0,
                            width: 100,
                            height: 24,
                        },
                    }],
                    splits: vec![],
                },
            },
        },
    ];

    for event in events {
        let value = serde_json::to_value(&event).expect("test precondition");
        assert!(value.get("event").is_none());
        assert_eq!(
            value["data"]["type"],
            serde_json::to_value(event.data.kind()).expect("test precondition")
        );
        let json = serde_json::to_string(&event).expect("test precondition");
        let restored: EventEnvelope = serde_json::from_str(&json).expect("test precondition");
        assert_eq!(restored, event);
    }
}

#[test]
fn subscribe_request_parses_parameterized_subscriptions() {
    let json = r#"
    {
        "id": "sub_1",
        "method": "events.subscribe",
        "params": {
            "subscriptions": [
                {
                    "type": "pane.output_matched",
                    "pane_id": "p_1_1",
                    "source": "recent",
                    "lines": 200,
                    "match": { "type": "substring", "value": "auth: received" }
                },
                {
                    "type": "pane.agent_status_changed",
                    "pane_id": "p_1_1",
                    "agent_status": "idle"
                },
                {
                    "type": "pane.scroll_changed",
                    "pane_id": "p_1_1"
                }
            ]
        }
    }
    "#;

    let request: Request = serde_json::from_str(json).expect("test precondition");
    let Method::EventsSubscribe(params) = request.method else {
        panic!("wrong method parsed");
    };
    assert_eq!(params.subscriptions.len(), 3);
    assert!(matches!(
        &params.subscriptions[0],
        Subscription::PaneOutputMatched {
            pane_id,
            source: ReadSource::Recent,
            lines: Some(200),
            r#match: OutputMatch::Substring { value },
            strip_ansi: true,
        } if pane_id == "p_1_1" && value == "auth: received"
    ));
    assert!(matches!(
        &params.subscriptions[1],
        Subscription::PaneAgentStatusChanged {
            pane_id,
            agent_status: Some(AgentStatus::Idle),
        } if pane_id == "p_1_1"
    ));
    assert!(matches!(
        &params.subscriptions[2],
        Subscription::PaneScrollChanged { pane_id } if pane_id == "p_1_1"
    ));
}

#[test]
fn subscription_event_envelope_round_trips() {
    let event = SubscriptionEventEnvelope {
        event: SubscriptionEventKind::PaneOutputMatched,
        data: SubscriptionEventData::PaneOutputMatched(PaneOutputMatchedEvent {
            pane_id: "p_1_1".into(),
            matched_line: "auth: received".into(),
            read: PaneReadResult {
                pane_id: "p_1_1".into(),
                workspace_id: "w_1".into(),
                tab_id: "t_1_1".into(),
                source: ReadSource::Recent,
                format: ReadFormat::Text,
                text: "auth: received\n".into(),
                revision: 0,
                truncated: false,
            },
        }),
    };

    let json = serde_json::to_string(&event).expect("test precondition");
    assert!(json.contains("\"event\":\"pane.output_matched\""));
    let restored: SubscriptionEventEnvelope =
        serde_json::from_str(&json).expect("test precondition");
    assert_eq!(restored, event);
}

#[test]
fn scroll_changed_subscription_event_round_trips() {
    let event = SubscriptionEventEnvelope {
        event: SubscriptionEventKind::ScrollChanged,
        data: SubscriptionEventData::ScrollChanged(PaneScrollChangedEvent {
            pane_id: "p_1_1".into(),
            workspace_id: "w_1".into(),
            scroll: PaneScrollInfo {
                offset_from_bottom: 12,
                max_offset_from_bottom: 240,
                viewport_rows: 30,
            },
        }),
    };

    let json = serde_json::to_string(&event).expect("test precondition");
    assert!(json.contains("\"event\":\"pane.scroll_changed\""));
    let restored: SubscriptionEventEnvelope =
        serde_json::from_str(&json).expect("test precondition");
    assert_eq!(restored, event);
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
            capabilities: Some(ServerCapabilities {
                detached_server_daemon: true,
                ssh_agent_registration: false,
            }),
        },
    };

    let json = serde_json::to_string(&response).expect("test precondition");
    let value: serde_json::Value = serde_json::from_str(&json).expect("test precondition");
    assert!(
        value["result"]["capabilities"]
            .get("surface_interest")
            .is_none()
    );
    assert!(
        value["result"]["capabilities"]
            .get("health_check")
            .is_none()
    );
    let restored: SuccessResponse = serde_json::from_str(&json).expect("test precondition");
    assert_eq!(restored, response);
}

#[test]
fn session_snapshot_request_and_response_round_trip() {
    let request = Request {
        id: "req_snapshot".into(),
        method: Method::SessionSnapshot(EmptyParams::default()),
    };
    let json = serde_json::to_string(&request).expect("test precondition");
    assert!(json.contains("\"method\":\"session.snapshot\""));
    let restored: Request = serde_json::from_str(&json).expect("test precondition");
    assert_eq!(restored, request);

    let response = SuccessResponse {
        id: "req_snapshot".into(),
        result: ResponseResult::SessionSnapshot {
            snapshot: Box::new(SessionSnapshot {
                version: "0.1.2".into(),
                focused_workspace_id: None,
                focused_tab_id: None,
                focused_pane_id: None,
                workspaces: Vec::new(),
                tabs: Vec::new(),
                panes: Vec::new(),
                layouts: Vec::new(),
                agents: Vec::new(),
            }),
        },
    };
    let json = serde_json::to_string(&response).expect("test precondition");
    assert!(json.contains("\"type\":\"session_snapshot\""));
    let restored: SuccessResponse = serde_json::from_str(&json).expect("test precondition");
    assert_eq!(restored, response);
}

#[test]
fn layout_export_apply_round_trip() {
    let root = LayoutNode::Split {
        direction: SplitDirection::Right,
        ratio: 0.6,
        first: Box::new(LayoutNode::Pane {
            pane: LayoutPane {
                label: Some("editor".into()),
                cwd: Some("/repo".into()),
                ..Default::default()
            },
        }),
        second: Box::new(LayoutNode::Pane {
            pane: LayoutPane {
                label: Some("tests".into()),
                command: Some(vec!["sh".into(), "-c".into(), "just test".into()]),
                env: HashMap::from([("ROLE".into(), "tests".into())]),
                ..Default::default()
            },
        }),
    };

    let export = Request {
        id: "layout_export".into(),
        method: Method::LayoutExport(LayoutExportParams {
            tab_id: Some("w1:1".into()),
            pane_id: None,
        }),
    };
    let json = serde_json::to_string(&export).expect("test precondition");
    assert!(json.contains("\"method\":\"layout.export\""));
    let restored: Request = serde_json::from_str(&json).expect("test precondition");
    assert_eq!(restored, export);

    let apply = Request {
        id: "layout_apply".into(),
        method: Method::LayoutApply(LayoutApplyParams {
            workspace_id: Some("w1".into()),
            tab_id: None,
            tab_label: Some("dev".into()),
            focus: true,
            root: root.clone(),
        }),
    };
    let json = serde_json::to_string(&apply).expect("test precondition");
    assert!(json.contains("\"method\":\"layout.apply\""));
    let restored: Request = serde_json::from_str(&json).expect("test precondition");
    assert_eq!(restored, apply);

    let response = SuccessResponse {
        id: "layout_export".into(),
        result: ResponseResult::LayoutExport {
            layout: LayoutDescription {
                workspace_id: "w1".into(),
                tab_id: "w1:1".into(),
                zoomed: false,
                focused_pane_id: "w1-1".into(),
                root,
            },
        },
    };
    let json = serde_json::to_string(&response).expect("test precondition");
    let restored: SuccessResponse = serde_json::from_str(&json).expect("test precondition");
    assert_eq!(restored, response);

    let response = SuccessResponse {
        id: "layout_ratio".into(),
        result: ResponseResult::LayoutSplitRatioSet {
            layout: LayoutDescription {
                workspace_id: "w1".into(),
                tab_id: "w1:1".into(),
                zoomed: false,
                focused_pane_id: "w1-1".into(),
                root: LayoutNode::Pane {
                    pane: LayoutPane {
                        pane_id: Some("w1-1".into()),
                        ..Default::default()
                    },
                },
            },
        },
    };
    let json = serde_json::to_string(&response).expect("test precondition");
    assert!(json.contains("\"type\":\"layout_split_ratio_set\""));
    let restored: SuccessResponse = serde_json::from_str(&json).expect("test precondition");
    assert_eq!(restored, response);
}

#[test]
fn authority_mutation_requests_round_trip() {
    let workspace_move = Request {
        id: "move_ws".into(),
        method: Method::WorkspaceMove(WorkspaceMoveParams {
            workspace_id: "w1".into(),
            insert_index: 2,
        }),
    };
    let json = serde_json::to_value(&workspace_move).expect("test precondition");
    assert_eq!(json["method"], "workspace.move");
    let restored: Request = serde_json::from_value(json).expect("test precondition");
    assert_eq!(restored, workspace_move);

    let workspace_move_block = Request {
        id: "move_ws_block".into(),
        method: Method::WorkspaceMoveBlock(WorkspaceMoveBlockParams {
            workspace_ids: vec!["w1".into(), "w2".into()],
            before_workspace_id: Some("w3".into()),
        }),
    };
    let json = serde_json::to_value(&workspace_move_block).expect("test precondition");
    assert_eq!(json["method"], "workspace.move_block");
    let restored: Request = serde_json::from_value(json).expect("test precondition");
    assert_eq!(restored, workspace_move_block);

    let tab_move = Request {
        id: "move_tab".into(),
        method: Method::TabMove(TabMoveParams {
            tab_id: "w1:1".into(),
            insert_index: 1,
        }),
    };
    let json = serde_json::to_value(&tab_move).expect("test precondition");
    assert_eq!(json["method"], "tab.move");
    let restored: Request = serde_json::from_value(json).expect("test precondition");
    assert_eq!(restored, tab_move);

    let pane_focus = Request {
        id: "focus_pane".into(),
        method: Method::PaneFocus(PaneTarget {
            pane_id: "w1:1".into(),
        }),
    };
    let json = serde_json::to_value(&pane_focus).expect("test precondition");
    assert_eq!(json["method"], "pane.focus");
    let restored: Request = serde_json::from_value(json).expect("test precondition");
    assert_eq!(restored, pane_focus);

    let split_ratio = Request {
        id: "set_ratio".into(),
        method: Method::LayoutSetSplitRatio(LayoutSetSplitRatioParams {
            tab_id: Some("w1:1".into()),
            pane_id: None,
            path: vec![false, true],
            ratio: 0.6,
        }),
    };
    let json = serde_json::to_value(&split_ratio).expect("test precondition");
    assert_eq!(json["method"], "layout.set_split_ratio");
    let restored: Request = serde_json::from_value(json).expect("test precondition");
    assert_eq!(restored, split_ratio);

    let subscription = Request {
        id: "sub_moves".into(),
        method: Method::EventsSubscribe(EventsSubscribeParams {
            subscriptions: vec![
                Subscription::WorkspaceMoved {},
                Subscription::WorkspaceReordered {},
                Subscription::TabMoved {},
                Subscription::LayoutUpdated {},
            ],
        }),
    };
    let json = serde_json::to_string(&subscription).expect("test precondition");
    assert!(json.contains("\"type\":\"workspace.moved\""));
    assert!(json.contains("\"type\":\"workspace.reordered\""));
    assert!(json.contains("\"type\":\"tab.moved\""));
    assert!(json.contains("\"type\":\"layout.updated\""));
    let restored: Request = serde_json::from_str(&json).expect("test precondition");
    assert_eq!(restored, subscription);
}

#[test]
fn create_response_round_trips_with_root_pane() {
    let response = SuccessResponse {
        id: "req_2".into(),
        result: ResponseResult::TabCreated {
            tab: TabInfo {
                tab_id: "w_1:2".into(),
                workspace_id: "w_1".into(),
                number: 2,
                label: "review".into(),
                focused: false,
                pane_count: 1,
                agent_status: AgentStatus::Idle,
            },
            root_pane: PaneInfo {
                pane_id: "w_1-3".into(),
                terminal_id: "term_example".into(),
                workspace_id: "w_1".into(),
                tab_id: "w_1:2".into(),
                focused: false,
                cwd: Some("/tmp/review".into()),
                foreground_cwd: None,
                restore_error: None,
                label: None,
                agent: None,
                title: None,
                terminal_title: None,
                terminal_title_stripped: None,
                display_agent: None,
                agent_status: AgentStatus::Idle,
                tokens: HashMap::new(),
                agent_session: None,
                scroll: None,
                revision: 0,
            },
        },
    };

    let json = serde_json::to_string(&response).expect("test precondition");
    assert!(json.contains("\"type\":\"tab_created\""));
    assert!(json.contains("\"root_pane\""));
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

#[test]
fn event_wait_parses_typed_match() {
    let json = r#"
    {
        "id": "req_9",
        "method": "events.wait",
        "params": {
            "match_event": {
                "event": "pane_agent_status_changed",
                "pane_id": "p_1",
                "agent_status": "idle"
            },
            "timeout_ms": 30000
        }
    }
    "#;

    let request: Request = serde_json::from_str(json).expect("test precondition");
    let Method::EventsWait(params) = request.method else {
        panic!("wrong method parsed");
    };
    assert_eq!(
        params.match_event,
        EventMatch::PaneAgentStatusChanged {
            pane_id: "p_1".into(),
            agent_status: AgentStatus::Idle,
        }
    );
}

#[test]
fn event_wait_rejects_matches_it_cannot_serve_at_parse_time() {
    // events.wait only matches agent status; other kinds must not parse and
    // then fail later with a runtime "unsupported" error.
    for event in ["workspace_created", "pane_closed", "pane_output_changed"] {
        let json = serde_json::json!({
            "id": "req_unsupported",
            "method": "events.wait",
            "params": { "match_event": { "event": event, "pane_id": "p_1" } }
        });
        assert!(
            serde_json::from_value::<Request>(json).is_err(),
            "{event} should be rejected"
        );
    }
}

#[test]
fn removed_never_emitted_subscriptions_do_not_parse() {
    let json = serde_json::json!({
        "id": "req_sub",
        "method": "events.subscribe",
        "params": { "subscriptions": [{ "type": "workspace.updated" }] }
    });
    assert!(serde_json::from_value::<Request>(json).is_err());
}
