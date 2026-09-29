use super::*;
use crate::app::Mode;
use crate::app::api_helpers::{METADATA_SOURCE_MAX_CHARS, METADATA_TTL_MAX_MS};
use crate::test_support::*;
use shepr_agent::detect::{Agent, AgentState};
use shepr_api::schema::{ErrorResponse, EventKind, SplitDirection, SuccessResponse};
use shepr_config::Config;
use shepr_mux::workspace::Workspace;

fn app_with_test_workspace() -> (App, String) {
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &Config::default(),
        crate::app::AppPolicy::Test,
        api_rx,
        shepr_api::EventHub::default(),
    );
    app.state.workspaces = vec![Workspace::test_new("metadata")];
    app.state.ensure_test_terminals();
    let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
    let public_pane_id = app.public_pane_id(0, pane_id).expect("test precondition");
    (app, public_pane_id)
}

#[test]
fn pane_input_set_changes_only_the_target_pane() {
    let (mut app, public_pane_id) = app_with_test_workspace();
    let target = app.state.workspaces[0].tabs()[0].root_pane();
    let other = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);

    let response = app.handle_pane_input_set(&PaneInputSetParams {
        pane_id: public_pane_id,
        right_click: shepr_api::schema::PaneRightClickTarget::Pane,
    });

    let response: SuccessResponse = crate::test_support::test_success(&response);
    assert!(matches!(response.result, ResponseResult::Ok {}));
    assert!(
        app.state.workspaces[0]
            .pane_state(target)
            .expect("test precondition")
            .right_click_passthrough
    );
    assert!(
        !app.state.workspaces[0]
            .pane_state(other)
            .expect("test precondition")
            .right_click_passthrough
    );
}

fn app_with_scrollback_runtime() -> (App, String, PaneId) {
    let (mut app, public_pane_id) = app_with_test_workspace();
    let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
    let lines = (0..20)
        .map(|line| format!("line {line:02}\n"))
        .collect::<String>();
    let runtime =
        shepr_mux::pane::PaneRuntime::test_with_scrollback_bytes(20, 5, 1000, lines.as_bytes());
    app.insert_test_runtime(pane_id, runtime);
    (app, public_pane_id, pane_id)
}

fn metadata_params(pane_id: String) -> PaneReportMetadataParams {
    PaneReportMetadataParams {
        pane_id,
        source: "user:metadata.test-1".into(),
        agent: None,
        applies_to_source: None,
        title: Some("activity".into()),
        display_agent: None,
        tokens: std::collections::HashMap::new(),
        clear_title: false,
        clear_display_agent: false,
        seq: None,
        ttl_ms: None,
    }
}

fn metadata_error_code(response: &ApiResult) -> ApiErrorCode {
    response
        .as_ref()
        .expect_err("request must fail")
        .code
        .clone()
}

#[tokio::test]
async fn api_clear_pane_mutates_endpoint_owned_history() {
    let (mut app, public_pane_id, pane_id) = app_with_scrollback_runtime();
    let request = shepr_api::schema::Request {
        id: "clear".into(),
        method: shepr_api::schema::Method::PaneClear(PaneTarget {
            pane_id: public_pane_id,
        }),
    };
    assert!(request.method.traits().mutates_ui);
    let response = app.handle_api_request(request);
    let success: SuccessResponse = crate::test_support::test_success(&response);
    assert_eq!(success.result, ResponseResult::Ok {});
    let runtime = app
        .state
        .runtime_for_pane_in_workspace(&app.terminal_runtimes, 0, pane_id)
        .expect("test precondition");
    assert_eq!(
        runtime
            .scroll_metrics()
            .expect("test precondition")
            .max_offset_from_bottom,
        0
    );
}

#[tokio::test]
async fn api_pane_get_exposes_scroll_metrics() {
    let (mut app, public_pane_id, pane_id) = app_with_scrollback_runtime();
    let runtime = app
        .state
        .runtime_for_pane_in_workspace(&app.terminal_runtimes, 0, pane_id)
        .expect("runtime");
    runtime.scroll_up(3);

    let response = app.handle_pane_get(&PaneTarget {
        pane_id: public_pane_id,
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneInfo { pane } = success.result else {
        panic!("expected pane info response");
    };
    let scroll = pane.scroll.expect("scroll metrics");
    assert_eq!(scroll.offset_from_bottom, 3);
    assert!(scroll.max_offset_from_bottom >= scroll.offset_from_bottom);
    assert_eq!(scroll.viewport_rows, 5);
}

#[tokio::test]
async fn api_pane_scroll_sets_and_clamps_endpoint_owned_history() {
    let (mut app, public_pane_id, pane_id) = app_with_scrollback_runtime();
    let runtime = app
        .state
        .runtime_for_pane_in_workspace(&app.terminal_runtimes, 0, pane_id)
        .expect("runtime");
    let max_offset = runtime
        .scroll_metrics()
        .expect("scroll metrics")
        .max_offset_from_bottom;

    let response = app.handle_pane_scroll(&PaneScrollParams {
        pane_id: public_pane_id,
        offset_from_bottom: u64::MAX,
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneInfo { pane } = success.result else {
        panic!("expected pane info response");
    };
    assert_eq!(
        pane.scroll.expect("scroll metrics").offset_from_bottom,
        max_offset as u64
    );
}

#[tokio::test]
async fn api_pane_selection_read_uses_endpoint_terminal_text() {
    let (mut app, public_pane_id) = app_with_test_workspace();
    let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
    app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_scrollback_bytes(20, 5, 1000, b"hello world"),
    );

    let runtime = app
        .state
        .runtime_for_pane_in_workspace(&app.terminal_runtimes, 0, pane_id)
        .expect("test precondition");
    let revision = runtime.content_seq();
    runtime.test_process_pty_bytes(b"\r\nagent is still working");
    assert_ne!(runtime.content_seq(), revision);
    let mut params = PaneSelectionReadParams {
        pane_id: public_pane_id.clone(),
        anchor: shepr_api::schema::PaneSelectionPoint {
            row: shepr_vt::AbsRow(0),
            col: 0,
        },
        cursor: shepr_api::schema::PaneSelectionPoint {
            row: shepr_vt::AbsRow(0),
            col: 4,
        },
        content_revision: Some(revision),
    };
    assert_eq!(
        app.pane_selection_text(&params)
            .expect_err("test precondition")
            .code,
        shepr_api::error::ApiErrorCode::StaleContent
    );
    params.content_revision = None;
    let response = app.handle_pane_selection_read(params);

    let success: SuccessResponse = crate::test_support::test_success(&response);
    assert_eq!(
        success.result,
        ResponseResult::PaneSelection {
            pane_id: public_pane_id,
            text: "hello".into(),
        }
    );
}

#[tokio::test]
async fn api_copy_motion_uses_endpoint_terminal_word_semantics() {
    let (mut app, public_pane_id) = app_with_test_workspace();
    let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
    app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_scrollback_bytes(20, 5, 1000, b"hello world"),
    );

    let response = app.handle_pane_copy_motion(PaneCopyMotionParams {
        pane_id: public_pane_id.clone(),
        cursor: shepr_api::schema::PaneTextPoint {
            row: shepr_vt::ScreenRow(0),
            col: 0,
        },
        motion: PaneCopyMotion::NextWordStart,
        content_revision: None,
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    assert_eq!(
        success.result,
        ResponseResult::PaneCopyMotion {
            pane_id: public_pane_id,
            cursor: shepr_api::schema::PaneTextPoint {
                row: shepr_vt::ScreenRow(0),
                col: 6
            },
            content_revision: 0,
        }
    );
}

#[tokio::test]
async fn api_paragraph_motion_preserves_the_copy_cursor_column() {
    let (mut app, public_pane_id) = app_with_test_workspace();
    let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
    app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_scrollback_bytes(20, 5, 1000, b"one\r\n\r\nthree"),
    );
    let response = app.handle_pane_copy_motion(PaneCopyMotionParams {
        pane_id: public_pane_id.clone(),
        cursor: PaneTextPoint {
            row: shepr_vt::ScreenRow(0),
            col: 2,
        },
        motion: PaneCopyMotion::NextParagraph,
        content_revision: None,
    });
    let success: SuccessResponse = crate::test_support::test_success(&response);
    assert_eq!(
        success.result,
        ResponseResult::PaneCopyMotion {
            pane_id: public_pane_id,
            cursor: PaneTextPoint {
                row: shepr_vt::ScreenRow(1),
                col: 2
            },
            content_revision: 0,
        }
    );
}

#[tokio::test]
async fn api_copy_search_uses_endpoint_terminal_matches_and_wraps() {
    let (mut app, public_pane_id) = app_with_test_workspace();
    let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
    app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_scrollback_bytes(20, 5, 1000, b"alpha beta alpha"),
    );

    let content_revision = app
        .state
        .runtime_for_pane_in_workspace(&app.terminal_runtimes, 0, pane_id)
        .expect("runtime")
        .content_seq();
    let response = app.handle_pane_copy_search(PaneCopySearchParams {
        pane_id: public_pane_id.clone(),
        query: "alpha".into(),
        direction: PaneCopySearchDirection::Forward,
        cursor: PaneTextPoint {
            row: shepr_vt::ScreenRow(0),
            col: 0,
        },
        content_revision,
        previous: None,
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneCopySearch {
        pane_id,
        matches,
        current,
        total,
        current_global,
        ..
    } = success.result
    else {
        panic!("expected copy search response");
    };
    assert_eq!(pane_id, public_pane_id);
    assert_eq!(matches.len(), 2);
    assert_eq!(
        matches[0].start,
        PaneTextPoint {
            row: shepr_vt::ScreenRow(0),
            col: 0
        }
    );
    assert_eq!(
        matches[1].start,
        PaneTextPoint {
            row: shepr_vt::ScreenRow(0),
            col: 11
        }
    );
    assert_eq!(current, Some(1));
    assert_eq!(current_global, Some(1));
    assert_eq!(total, 2);
}

#[tokio::test]
async fn api_copy_search_bounds_returned_matches_but_keeps_exact_total() {
    let (mut app, public_pane_id) = app_with_test_workspace();
    let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
    let text = "a ".repeat(1500);
    app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_scrollback_bytes(200, 20, 4000, text.as_bytes()),
    );
    let content_revision = app
        .state
        .runtime_for_pane_in_workspace(&app.terminal_runtimes, 0, pane_id)
        .expect("runtime")
        .content_seq();

    let response = app.handle_pane_copy_search(PaneCopySearchParams {
        pane_id: public_pane_id,
        query: "a".into(),
        direction: PaneCopySearchDirection::Forward,
        cursor: PaneTextPoint {
            row: shepr_vt::ScreenRow(0),
            col: 0,
        },
        content_revision,
        previous: None,
    });
    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneCopySearch { matches, total, .. } = success.result else {
        panic!("expected copy search response");
    };
    assert_eq!(total, 1500);
    assert_eq!(matches.len(), 1024);
}

#[tokio::test]
async fn api_copy_search_rejects_stale_content_revision() {
    let (mut app, public_pane_id) = app_with_test_workspace();
    let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
    app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_scrollback_bytes(20, 5, 1000, b"alpha beta"),
    );
    let response = app.handle_pane_copy_search(PaneCopySearchParams {
        pane_id: public_pane_id,
        query: "alpha".into(),
        direction: PaneCopySearchDirection::Forward,
        cursor: PaneTextPoint {
            row: shepr_vt::ScreenRow(0),
            col: 0,
        },
        content_revision: 2,
        previous: None,
    });
    assert!(crate::test_support::test_json(&response).contains("stale_content"));
}

#[tokio::test]
async fn api_pane_read_reports_when_older_rows_are_omitted() {
    let (mut app, public_pane_id, _pane_id) = app_with_scrollback_runtime();

    let response = app.handle_pane_read(&PaneReadParams {
        pane_id: public_pane_id,
        source: shepr_api::schema::ReadSource::Recent,
        lines: Some(2),
        format: shepr_api::schema::ReadFormat::Text,
        strip_ansi: true,
        intent: shepr_api::schema::ReadIntent::Interactive,
    });
    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneRead { read } = success.result else {
        panic!("expected pane read response");
    };
    assert!(read.text.contains("line 19"));
    assert!(read.truncated);
}

#[tokio::test]
async fn api_pane_read_honours_strip_ansi_and_reports_content_revision() {
    let (mut app, public_pane_id, pane_id) = app_with_scrollback_runtime();
    let read = |app: &mut App, strip_ansi: bool, lines: Option<u32>| {
        app.handle_pane_read(&PaneReadParams {
            pane_id: public_pane_id.clone(),
            source: shepr_api::schema::ReadSource::Recent,
            lines,
            format: shepr_api::schema::ReadFormat::Text,
            strip_ansi,
            intent: shepr_api::schema::ReadIntent::Interactive,
        })
    };

    let kept = read(&mut app, false, Some(2));
    let success: SuccessResponse = crate::test_support::test_success(&kept);
    let ResponseResult::PaneRead { read: kept } = success.result else {
        panic!("expected pane read response");
    };
    assert_eq!(kept.format, shepr_api::schema::ReadFormat::Ansi);
    assert_eq!(
        kept.revision,
        app.lookup_runtime(0, pane_id)
            .expect("test precondition")
            .0
            .content_seq()
    );

    let stripped = read(&mut app, true, Some(2));
    let success: SuccessResponse = crate::test_support::test_success(&stripped);
    let ResponseResult::PaneRead { read: stripped } = success.result else {
        panic!("expected pane read response");
    };
    assert_eq!(stripped.format, shepr_api::schema::ReadFormat::Text);

    let oversized = read(
        &mut app,
        true,
        Some(crate::app::api_helpers::MAX_READ_LINES + 1),
    );
    let error: ErrorResponse = crate::test_support::test_error(&oversized);
    assert_eq!(error.error.code, "invalid_lines");
}

#[test]
fn api_pane_rename_emits_pane_updated() {
    let (mut app, public_pane_id) = app_with_test_workspace();

    let response = app.handle_pane_rename(PaneRenameParams {
        pane_id: public_pane_id.clone(),
        label: Some("build".into()),
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    assert!(matches!(success.result, ResponseResult::PaneInfo { .. }));
    assert!(
        app.event_hub
            .events_after(0)
            .iter()
            .any(|(_, event)| matches!(
                &event.data,
                EventData::PaneUpdated { pane }
                    if pane.pane_id == public_pane_id && pane.label.as_deref() == Some("build")
            ))
    );
}

fn app_with_workspace() -> App {
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &Config::default(),
        crate::app::AppPolicy::Test,
        api_rx,
        shepr_api::EventHub::default(),
    );
    app.state.workspaces = vec![Workspace::test_new("issue")];
    app.state.ensure_test_terminals();
    app
}

fn seed_terminal_states(app: &mut App) {
    for ws in &app.state.workspaces {
        for tab in ws.tabs() {
            for pane in tab.panes().values() {
                app.state
                    .terminals
                    .entry(pane.attached_terminal_id.clone())
                    .or_insert_with(|| {
                        shepr_mux::terminal::TerminalState::new(
                            pane.attached_terminal_id.clone(),
                            std::path::PathBuf::from("/shepr-test"),
                        )
                    });
            }
        }
    }
}

#[test]
fn api_pane_close_of_last_pane_closes_workspace() {
    let mut app = app_with_workspace();
    let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
    app.state
        .public_pane_id_aliases
        .insert(shepr_protocol::PublicPaneId::new("wOLD", 1), pane_id);
    let public_pane_id = app.public_pane_id(0, pane_id).expect("test precondition");

    let response = app.handle_pane_close(&PaneTarget {
        pane_id: public_pane_id,
    });

    let _: SuccessResponse = crate::test_support::test_success(&response);
    assert!(app.state.workspaces.is_empty());
    assert!(
        !app.state
            .public_pane_id_aliases
            .contains_key(&shepr_protocol::PublicPaneId::new("wOLD", 1))
    );
    assert_eq!(
        app.event_hub
            .events_after(0)
            .iter()
            .map(|(_, event)| event.data.kind())
            .collect::<Vec<_>>(),
        [
            EventKind::PaneClosed,
            EventKind::TabClosed,
            EventKind::WorkspaceClosed
        ]
    );
}

#[test]
fn api_pane_close_of_a_tabs_last_pane_announces_the_tab() {
    let mut app = app_with_workspace();
    app.state.workspaces[0].test_add_tab(Some("survivor"));
    app.state.ensure_test_terminals();
    let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
    let public_pane_id = app.public_pane_id(0, pane_id).expect("test precondition");
    let tab_id = app.public_tab_id(0, 0).expect("test precondition");

    let response = app.handle_pane_close(&PaneTarget {
        pane_id: public_pane_id.clone(),
    });

    let _: SuccessResponse = crate::test_support::test_success(&response);
    assert_eq!(app.state.workspaces[0].tabs().len(), 1);
    let events = app.event_hub.events_after(0);
    assert_eq!(events.len(), 2);
    assert!(matches!(
        &events[0].1.data,
        EventData::PaneClosed { pane_id, .. } if pane_id == &public_pane_id
    ));
    assert!(matches!(
        &events[1].1.data,
        EventData::TabClosed { tab_id: closed, .. } if closed == &tab_id
    ));
}

#[test]
fn api_pane_current_prefers_caller_pane_id() {
    let mut app = app_with_workspace();
    app.state.set_active_index(Some(0));
    app.state.set_selected_index(Some(0));
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let right = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
    app.state.ensure_test_terminals();
    app.state.workspaces[0].focus_pane_in_tab(0, root);
    let root_public = app.public_pane_id(0, root).expect("test precondition");
    let right_public = app.public_pane_id(0, right).expect("test precondition");

    let response = app.handle_pane_current(&shepr_api::schema::PaneCurrentParams {
        caller_pane_id: Some(right_public.clone()),
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneCurrent { pane } = success.result else {
        panic!("expected pane current response");
    };
    assert_eq!(pane.pane_id, right_public);
    assert!(!pane.focused);
    assert_eq!(app.state.workspaces[0].focused_pane_id(), root);
    assert_ne!(pane.pane_id, root_public);
}

#[test]
fn api_pane_current_falls_back_to_focused_pane() {
    let mut app = app_with_workspace();
    app.state.set_active_index(Some(0));
    app.state.set_selected_index(Some(0));
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    app.state.workspaces[0].focus_pane_in_tab(0, root);
    let root_public = app.public_pane_id(0, root).expect("test precondition");

    let response = app.handle_pane_current(&shepr_api::schema::PaneCurrentParams::default());

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneCurrent { pane } = success.result else {
        panic!("expected pane current response");
    };
    assert_eq!(pane.pane_id, root_public);
    assert!(pane.focused);
}

#[test]
fn api_pane_current_dispatches_through_socket_request() {
    let mut app = app_with_workspace();
    app.state.set_active_index(Some(0));
    app.state.set_selected_index(Some(0));
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let root_public = app.public_pane_id(0, root).expect("test precondition");

    let response = app.handle_api_request(shepr_api::schema::Request {
        id: "req".into(),
        method: shepr_api::schema::Method::PaneCurrent(
            shepr_api::schema::PaneCurrentParams::default(),
        ),
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneCurrent { pane } = success.result else {
        panic!("expected pane current response");
    };
    assert_eq!(pane.pane_id, root_public);
}

#[test]
fn api_pane_current_reports_invalid_caller_pane_id() {
    let mut app = app_with_workspace();

    let response = app.handle_pane_current(&shepr_api::schema::PaneCurrentParams {
        caller_pane_id: Some("missing".into()),
    });

    assert_eq!(metadata_error_code(&response), ApiErrorCode::PaneNotFound);
}

#[test]
fn api_pane_current_reports_no_active_pane() {
    let mut app = app_with_workspace();
    app.state.set_active_index(None);

    let response = app.handle_pane_current(&shepr_api::schema::PaneCurrentParams::default());

    assert_eq!(metadata_error_code(&response), ApiErrorCode::PaneNotFound);
}

#[test]
fn api_pane_swap_explicit_source_and_target_preserves_focus_and_returns_layout() {
    let mut app = app_with_workspace();
    let source = app.state.workspaces[0].tabs()[0].root_pane();
    let target = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(0, source);
    app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 20);
    let source_public = app.public_pane_id(0, source).expect("test precondition");
    let target_public = app.public_pane_id(0, target).expect("test precondition");

    let response = app.handle_pane_swap(PaneSwapParams {
        source_pane_id: Some(source_public.clone()),
        target_pane_id: Some(target_public.clone()),
        ..PaneSwapParams::default()
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneSwap { swap } = success.result else {
        panic!("expected pane swap response");
    };
    assert!(swap.changed);
    assert_eq!(swap.reason, None);
    assert_eq!(swap.source_pane_id, source_public);
    assert_eq!(swap.target_pane_id, Some(target_public));
    assert_eq!(swap.focused_pane_id, swap.source_pane_id);
    assert_eq!(swap.layout.focused_pane_id, swap.source_pane_id);
    assert_eq!(swap.layout.panes.len(), 2);
    assert_eq!(app.state.workspaces[0].focused_pane_id(), source);
}

#[test]
fn api_pane_swap_direction_no_neighbor_returns_unchanged_layout() {
    let mut app = app_with_workspace();
    let source = app.state.workspaces[0].tabs()[0].root_pane();
    app.state.workspaces[0].focus_pane_in_tab(0, source);
    app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 20);
    let source_public = app.public_pane_id(0, source).expect("test precondition");

    let response = app.handle_pane_swap(PaneSwapParams {
        pane_id: Some(source_public.clone()),
        direction: Some(PaneDirection::Left),
        ..PaneSwapParams::default()
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneSwap { swap } = success.result else {
        panic!("expected pane swap response");
    };
    assert!(!swap.changed);
    assert_eq!(swap.reason, Some(PaneSwapReason::NoNeighbor));
    assert_eq!(swap.source_pane_id, source_public);
    assert_eq!(swap.target_pane_id, None);
    assert_eq!(swap.layout.panes.len(), 1);
    assert!(app.event_hub.events_after(0).is_empty());
}

#[test]
fn api_pane_swap_explicit_missing_target_returns_not_found_noop() {
    let mut app = app_with_workspace();
    let source = app.state.workspaces[0].tabs()[0].root_pane();
    let source_public = app.public_pane_id(0, source).expect("test precondition");

    let response = app.handle_pane_swap(PaneSwapParams {
        source_pane_id: Some(source_public.clone()),
        target_pane_id: Some("missing-pane".into()),
        ..PaneSwapParams::default()
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneSwap { swap } = success.result else {
        panic!("expected pane swap response");
    };
    assert!(!swap.changed);
    assert_eq!(swap.reason, Some(PaneSwapReason::NotFound));
    assert_eq!(swap.source_pane_id, source_public);
    assert_eq!(swap.target_pane_id, Some("missing-pane".into()));
    assert_eq!(swap.layout.panes.len(), 1);
}

#[test]
fn api_pane_swap_explicit_missing_source_returns_not_found_noop() {
    let mut app = app_with_workspace();
    let target = app.state.workspaces[0].tabs()[0].root_pane();
    let target_public = app.public_pane_id(0, target).expect("test precondition");

    let response = app.handle_pane_swap(PaneSwapParams {
        source_pane_id: Some("missing-pane".into()),
        target_pane_id: Some(target_public.clone()),
        ..PaneSwapParams::default()
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneSwap { swap } = success.result else {
        panic!("expected pane swap response");
    };
    assert!(!swap.changed);
    assert_eq!(swap.reason, Some(PaneSwapReason::NotFound));
    assert_eq!(swap.source_pane_id, "missing-pane");
    assert_eq!(swap.target_pane_id, Some(target_public));
    assert_eq!(swap.layout.panes.len(), 1);
}

#[test]
fn api_pane_swap_explicit_cross_workspace_preserves_target_id() {
    let mut app = app_with_workspace();
    app.state.workspaces.push(Workspace::test_new("other"));
    let source = app.state.workspaces[0].tabs()[0].root_pane();
    let target = app.state.workspaces[1].tabs()[0].root_pane();
    let source_public = app.public_pane_id(0, source).expect("test precondition");
    let target_public = app.public_pane_id(1, target).expect("test precondition");

    let response = app.handle_pane_swap(PaneSwapParams {
        source_pane_id: Some(source_public.clone()),
        target_pane_id: Some(target_public.clone()),
        ..PaneSwapParams::default()
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneSwap { swap } = success.result else {
        panic!("expected pane swap response");
    };
    assert!(!swap.changed);
    assert_eq!(swap.reason, Some(PaneSwapReason::CrossTab));
    assert_eq!(swap.source_pane_id, source_public);
    assert_eq!(swap.target_pane_id, Some(target_public));
    assert_eq!(
        swap.layout.workspace_id,
        app.public_workspace_id(0).expect("test precondition")
    );
}

#[test]
fn api_pane_move_to_existing_tab_preserves_internal_pane_and_terminal() {
    let mut app = app_with_workspace();
    let source = app.state.workspaces[0].tabs()[0].root_pane();
    let source_terminal = app.state.workspaces[0].tabs()[0]
        .terminal_id(source)
        .expect("test precondition")
        .clone();
    let target_tab = app.state.workspaces[0].test_add_tab(Some("target"));
    let target = app.state.workspaces[0].tabs()[target_tab].root_pane();
    seed_terminal_states(&mut app);
    let source_public = app.public_pane_id(0, source).expect("test precondition");
    let source_tab_public = app.public_tab_id(0, 0).expect("test precondition");
    let target_public = app.public_pane_id(0, target).expect("test precondition");
    let target_tab_public = app.public_tab_id(0, target_tab).expect("test precondition");

    let response = app.handle_pane_move(PaneMoveParams {
        pane_id: source_public.clone(),
        destination: PaneMoveDestination::Tab {
            tab_id: target_tab_public.clone(),
            target_pane_id: Some(target_public),
            split: SplitDirection::Right,
            ratio: Some(0.25),
        },
        focus: true,
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneMove { move_result } = success.result else {
        panic!("expected pane move response");
    };
    assert!(move_result.changed);
    assert_eq!(move_result.reason, None);
    assert_eq!(move_result.previous_pane_id, source_public);
    assert_eq!(move_result.previous_tab_id, source_tab_public);
    assert_eq!(move_result.pane.pane_id, move_result.previous_pane_id);
    assert_eq!(move_result.pane.tab_id, target_tab_public);
    assert_eq!(move_result.pane.terminal_id, source_terminal.to_string());
    assert_eq!(move_result.closed_tab_id, Some(source_tab_public));
    assert_eq!(move_result.closed_workspace_id, None);
    assert_eq!(move_result.target_layout.panes.len(), 2);
    assert_eq!(app.state.workspaces[0].tabs().len(), 1);
    assert_eq!(app.state.workspaces[0].tabs()[0].layout().focused(), source);
    assert_eq!(
        app.state.workspaces[0].tabs()[0].terminal_id(source),
        Some(&source_terminal)
    );
}
#[test]
fn api_pane_move_to_existing_tab_across_workspace_reassigns_public_pane_id() {
    let mut app = app_with_workspace();
    app.state.workspaces.push(Workspace::test_new("other"));
    let source = app.state.workspaces[0].tabs()[0].root_pane();
    let source_terminal = app.state.workspaces[0].tabs()[0]
        .terminal_id(source)
        .expect("test precondition")
        .clone();
    let target = app.state.workspaces[1].tabs()[0].root_pane();
    seed_terminal_states(&mut app);
    app.state
        .terminals
        .get_mut(&source_terminal)
        .expect("test precondition")
        .set_detected_state(Some(Agent::Pi), AgentState::Idle);
    let previous_pane_id = app.public_pane_id(0, source).expect("test precondition");
    let previous_workspace_id = app.public_workspace_id(0).expect("test precondition");
    let target_workspace_id = app.public_workspace_id(1).expect("test precondition");
    let target_tab_id = app.public_tab_id(1, 0).expect("test precondition");
    let target_pane_id = app.public_pane_id(1, target).expect("test precondition");

    let response = app.handle_pane_move(PaneMoveParams {
        pane_id: previous_pane_id.clone(),
        destination: PaneMoveDestination::Tab {
            tab_id: target_tab_id.clone(),
            target_pane_id: Some(target_pane_id),
            split: SplitDirection::Down,
            ratio: None,
        },
        focus: false,
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneMove { move_result } = success.result else {
        panic!("expected pane move response");
    };
    assert!(move_result.changed);
    assert_eq!(move_result.previous_pane_id, previous_pane_id);
    assert_eq!(move_result.previous_workspace_id, previous_workspace_id);
    assert_eq!(move_result.closed_workspace_id, Some(previous_workspace_id));
    assert_ne!(move_result.pane.pane_id, move_result.previous_pane_id);
    assert!(
        move_result
            .pane
            .pane_id
            .starts_with(&format!("{target_workspace_id}:p"))
    );
    assert_eq!(move_result.pane.workspace_id, target_workspace_id);
    assert_eq!(move_result.pane.tab_id, target_tab_id);
    assert_eq!(move_result.pane.terminal_id, source_terminal.to_string());
    assert_eq!(app.state.workspaces.len(), 1);
    assert_eq!(
        app.state.workspaces[0].tabs()[0].terminal_id(source),
        Some(&source_terminal)
    );
    assert_eq!(app.parse_pane_id(&previous_pane_id), Some((0, source)));
    assert!(matches!(
        app.resolve_agent_target(&previous_pane_id),
        Err(crate::app::terminal_targets::TerminalTargetError::NotFound { .. })
    ));
    assert!(app.resolve_agent_target(&move_result.pane.pane_id).is_ok());
}

#[test]
fn api_pane_move_target_tab_id_survives_source_workspace_removal() {
    let mut app = app_with_workspace();
    app.state.workspaces.push(Workspace::test_new("other"));
    let source = app.state.workspaces[0].tabs()[0].root_pane();
    let source_terminal = app.state.workspaces[0].tabs()[0]
        .terminal_id(source)
        .expect("test precondition")
        .clone();
    let target = app.state.workspaces[1].tabs()[0].root_pane();
    seed_terminal_states(&mut app);
    let source_workspace_id = app.public_workspace_id(0).expect("test precondition");
    let target_workspace_id = app.public_workspace_id(1).expect("test precondition");
    let target_tab_id = app.public_tab_id(1, 0).expect("test precondition");
    let source_public = app.public_pane_id(0, source).expect("test precondition");
    let target_public = app.public_pane_id(1, target).expect("test precondition");

    let response = app.handle_pane_move(PaneMoveParams {
        pane_id: source_public,
        destination: PaneMoveDestination::Tab {
            tab_id: target_tab_id.clone(),
            target_pane_id: Some(target_public),
            split: SplitDirection::Right,
            ratio: None,
        },
        focus: true,
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneMove { move_result } = success.result else {
        panic!("expected pane move response");
    };
    assert!(move_result.changed);
    assert_eq!(app.state.workspaces.len(), 1);
    assert_eq!(move_result.closed_workspace_id, Some(source_workspace_id));
    assert_eq!(move_result.pane.workspace_id, target_workspace_id);
    assert_eq!(move_result.pane.tab_id, target_tab_id);
    assert_eq!(move_result.pane.terminal_id, source_terminal.to_string());
    assert_eq!(
        app.state.workspaces[0].tabs()[0].terminal_id(source),
        Some(&source_terminal)
    );
}

#[test]
fn api_pane_move_to_new_tab_creates_tab_without_spawning_terminal() {
    let mut app = app_with_workspace();
    let source = app.state.workspaces[0].tabs()[0].root_pane();
    let right = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
    let source_terminal = app.state.workspaces[0].tabs()[0]
        .terminal_id(source)
        .expect("test precondition")
        .clone();
    seed_terminal_states(&mut app);
    let source_public = app.public_pane_id(0, source).expect("test precondition");

    let response = app.handle_pane_move(PaneMoveParams {
        pane_id: source_public.clone(),
        destination: PaneMoveDestination::NewTab {
            workspace_id: None,
            label: Some("moved".into()),
        },
        focus: true,
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneMove { move_result } = success.result else {
        panic!("expected pane move response");
    };
    assert!(move_result.changed);
    assert_eq!(
        move_result
            .created_tab
            .as_ref()
            .map(|tab| tab.label.as_str()),
        Some("moved")
    );
    assert_eq!(
        move_result.created_tab.as_ref().map(|tab| tab.focused),
        Some(true)
    );
    assert_eq!(move_result.closed_tab_id, None);
    assert_eq!(move_result.pane.pane_id, source_public);
    assert_eq!(move_result.pane.terminal_id, source_terminal.to_string());
    assert_eq!(app.state.workspaces[0].tabs().len(), 2);
    assert!(
        app.state.workspaces[0].tabs()[0]
            .terminal_id(right)
            .is_some()
    );
    assert_eq!(
        app.state.workspaces[0].tabs()[1].terminal_id(source),
        Some(&source_terminal)
    );
    let envelopes = app.event_hub.events_after(0);
    let events: Vec<_> = envelopes
        .iter()
        .map(|(_, envelope)| envelope.data.kind())
        .collect();
    assert_eq!(
        events,
        vec![
            EventKind::TabCreated,
            EventKind::PaneMoved,
            EventKind::LayoutUpdated,
            EventKind::LayoutUpdated,
        ]
    );
    match &envelopes[0].1.data {
        EventData::TabCreated { tab } => assert!(tab.focused),
        other => panic!("expected tab created event, got {other:?}"),
    }
    assert!(matches!(
        &envelopes[2].1.data,
        EventData::LayoutUpdated { layout }
            if layout.tab_id == app.public_tab_id(0, 0).expect("test precondition")
    ));
    assert!(matches!(
        &envelopes[3].1.data,
        EventData::LayoutUpdated { layout }
            if layout.tab_id == app.public_tab_id(0, 1).expect("test precondition")
    ));
}

#[tokio::test]
async fn api_pane_move_only_pane_to_new_tab_preserves_runtime_registry() {
    let mut app = app_with_workspace();
    let source = app.state.workspaces[0].tabs()[0].root_pane();
    seed_terminal_states(&mut app);
    app.insert_test_runtime(
        source,
        shepr_mux::pane::PaneRuntime::test_with_screen_bytes(20, 5, b"moved"),
    );
    let runtime = std::ptr::from_ref(app.test_runtime(source));
    let source_public = app.public_pane_id(0, source).expect("test precondition");
    let source_tab_public = app.public_tab_id(0, 0).expect("test precondition");

    let response = app.handle_pane_move(PaneMoveParams {
        pane_id: source_public.clone(),
        destination: PaneMoveDestination::NewTab {
            workspace_id: None,
            label: Some("moved".into()),
        },
        focus: true,
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneMove { move_result } = success.result else {
        panic!("expected pane move response");
    };
    assert!(move_result.changed);
    assert_eq!(std::ptr::from_ref(app.test_runtime(source)), runtime);
    // The workspace keeps one live tab: the old tab closes and its
    // replacement, under a new public number, holds the pane.
    assert_eq!(move_result.closed_tab_id, Some(source_tab_public.clone()));
    assert_eq!(move_result.closed_workspace_id, None);
    assert_eq!(move_result.source_layout, None);
    assert_eq!(move_result.pane.pane_id, source_public);
    let created_tab = move_result.created_tab.expect("a tab is created");
    assert_eq!(created_tab.label, "moved");
    assert_ne!(created_tab.tab_id, source_tab_public);
    assert_eq!(app.state.workspaces.len(), 1);
    assert_eq!(app.state.workspaces[0].tabs().len(), 1);
    assert_eq!(app.state.workspaces[0].tabs()[0].root_pane(), source);
    app.state.workspaces[0].assert_invariants_for_test();
}

#[test]
fn api_pane_move_to_new_workspace_closes_empty_source_workspace() {
    let mut app = app_with_workspace();
    let source = app.state.workspaces[0].tabs()[0].root_pane();
    let source_terminal = app.state.workspaces[0].tabs()[0]
        .terminal_id(source)
        .expect("test precondition")
        .clone();
    seed_terminal_states(&mut app);
    let source_public = app.public_pane_id(0, source).expect("test precondition");
    let source_workspace = app.public_workspace_id(0).expect("test precondition");

    let response = app.handle_pane_move(PaneMoveParams {
        pane_id: source_public.clone(),
        destination: PaneMoveDestination::NewWorkspace {
            label: Some("promoted".into()),
            tab_label: Some("main".into()),
        },
        focus: true,
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneMove { move_result } = success.result else {
        panic!("expected pane move response");
    };
    assert!(move_result.changed);
    assert_eq!(move_result.closed_workspace_id, Some(source_workspace));
    assert_eq!(
        move_result
            .created_workspace
            .as_ref()
            .map(|ws| ws.label.as_str()),
        Some("promoted")
    );
    assert_eq!(
        move_result.created_workspace.as_ref().map(|ws| ws.focused),
        Some(true)
    );
    assert_eq!(
        move_result
            .created_tab
            .as_ref()
            .map(|tab| tab.label.as_str()),
        Some("main")
    );
    assert_eq!(
        move_result.created_tab.as_ref().map(|tab| tab.focused),
        Some(true)
    );
    assert_ne!(move_result.pane.pane_id, source_public);
    assert_eq!(move_result.pane.terminal_id, source_terminal.to_string());
    assert_eq!(app.state.workspaces.len(), 1);
    assert_eq!(
        app.state.workspaces[0].tabs()[0].terminal_id(source),
        Some(&source_terminal)
    );
    let envelopes = app.event_hub.events_after(0);
    let events: Vec<_> = envelopes
        .iter()
        .map(|(_, envelope)| envelope.data.kind())
        .collect();
    assert_eq!(
        events,
        vec![
            EventKind::TabClosed,
            EventKind::WorkspaceClosed,
            EventKind::WorkspaceCreated,
            EventKind::TabCreated,
            EventKind::PaneMoved,
            EventKind::LayoutUpdated,
        ]
    );
    match &envelopes[2].1.data {
        EventData::WorkspaceCreated { workspace } => assert!(workspace.focused),
        other => panic!("expected workspace created event, got {other:?}"),
    }
    match &envelopes[3].1.data {
        EventData::TabCreated { tab } => assert!(tab.focused),
        other => panic!("expected tab created event, got {other:?}"),
    }
    match &envelopes[5].1.data {
        EventData::LayoutUpdated { layout } => assert_eq!(
            layout.tab_id,
            app.public_tab_id(0, 0)
                .expect("created workspace should have a first tab")
        ),
        other => panic!("expected layout updated event, got {other:?}"),
    }
}

#[test]
fn api_pane_move_same_tab_returns_same_tab_noop() {
    let mut app = app_with_workspace();
    let source = app.state.workspaces[0].tabs()[0].root_pane();
    seed_terminal_states(&mut app);
    let source_public = app.public_pane_id(0, source).expect("test precondition");
    let source_tab = app.public_tab_id(0, 0).expect("test precondition");

    let response = app.handle_pane_move(PaneMoveParams {
        pane_id: source_public,
        destination: PaneMoveDestination::Tab {
            tab_id: source_tab,
            target_pane_id: None,
            split: SplitDirection::Right,
            ratio: None,
        },
        focus: true,
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneMove { move_result } = success.result else {
        panic!("expected pane move response");
    };
    assert!(!move_result.changed);
    assert_eq!(move_result.reason, Some(PaneMoveReason::SameTab));
    assert_eq!(app.state.workspaces[0].tabs().len(), 1);
}

#[test]
fn api_pane_move_rejects_target_pane_outside_target_tab() {
    let mut app = app_with_workspace();
    let source = app.state.workspaces[0].tabs()[0].root_pane();
    let target_tab = app.state.workspaces[0].test_add_tab(Some("target"));
    let other_tab = app.state.workspaces[0].test_add_tab(Some("other"));
    seed_terminal_states(&mut app);
    let source_public = app.public_pane_id(0, source).expect("test precondition");
    let target_tab_public = app.public_tab_id(0, target_tab).expect("test precondition");
    let wrong_target = app
        .public_pane_id(0, app.state.workspaces[0].tabs()[other_tab].root_pane())
        .expect("test precondition");

    let response = app.handle_pane_move(PaneMoveParams {
        pane_id: source_public,
        destination: PaneMoveDestination::Tab {
            tab_id: target_tab_public,
            target_pane_id: Some(wrong_target),
            split: SplitDirection::Right,
            ratio: None,
        },
        focus: true,
    });

    let error: shepr_api::schema::ErrorResponse =
        serde_json::from_str(&crate::test_support::test_json(&response))
            .expect("test precondition");
    assert_eq!(error.error.code, "target_pane_not_found");
    assert_eq!(app.state.workspaces[0].tabs().len(), 3);
}

#[test]
fn api_pane_move_existing_tab_no_focus_preserves_previous_target_focus() {
    let mut app = app_with_workspace();
    let source = app.state.workspaces[0].tabs()[0].root_pane();
    let target_tab = app.state.workspaces[0].test_add_tab(Some("target"));
    let previously_focused = app.state.workspaces[0].tabs()[target_tab].root_pane();
    app.state.workspaces[0].switch_tab(target_tab);
    let explicit_target =
        app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(target_tab, previously_focused);
    seed_terminal_states(&mut app);
    let source_public = app.public_pane_id(0, source).expect("test precondition");
    let target_tab_public = app.public_tab_id(0, target_tab).expect("test precondition");
    let explicit_target_public = app
        .public_pane_id(0, explicit_target)
        .expect("test precondition");
    let previously_focused_public = app
        .public_pane_id(0, previously_focused)
        .expect("test precondition");

    let response = app.handle_pane_move(PaneMoveParams {
        pane_id: source_public,
        destination: PaneMoveDestination::Tab {
            tab_id: target_tab_public,
            target_pane_id: Some(explicit_target_public),
            split: SplitDirection::Right,
            ratio: None,
        },
        focus: false,
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneMove { move_result } = success.result else {
        panic!("expected pane move response");
    };
    assert!(move_result.changed);
    assert_eq!(move_result.focused_pane_id, previously_focused_public);
    assert_eq!(
        app.state.workspaces[0].tabs()[0].layout().focused(),
        previously_focused
    );
}

#[test]
fn api_pane_move_recovery_restores_removed_source_workspace() {
    let mut app = app_with_workspace();
    let source = app.state.workspaces[0].tabs()[0].root_pane();
    let source_terminal = app.state.workspaces[0].tabs()[0]
        .terminal_id(source)
        .expect("test precondition")
        .clone();
    let previous_workspace_id = app.public_workspace_id(0).expect("test precondition");
    let context = PaneMoveRecoveryContext {
        source_ws_idx: 0,
        previous_workspace_id: previous_workspace_id.clone(),
        previous_workspace_label: app.state.workspaces[0].custom_name.clone(),
        previous_tab_label: app.state.workspaces[0].tabs()[0]
            .custom_name()
            .map(str::to_string),
        identity_cwd: app.state.workspaces[0].identity_cwd.clone(),
    };
    let Ok(moved) = app.state.workspaces.remove(0).into_only_pane() else {
        panic!("source pane should be movable");
    };
    app.state.set_active_index(None);
    app.state.set_selected_index(Some(0));

    app.recover_failed_pane_move(context, moved);

    assert_eq!(app.state.workspaces.len(), 1);
    assert_eq!(app.state.workspaces[0].id, previous_workspace_id);
    assert_eq!(
        app.state.workspaces[0].tabs()[0].terminal_id(source),
        Some(&source_terminal)
    );
    assert_eq!(
        app.parse_pane_id(&format!("{previous_workspace_id}:p1")),
        Some((0, source))
    );
}

#[test]
fn api_pane_move_to_zoomed_target_returns_target_layout() {
    let mut app = app_with_workspace();
    let source = app.state.workspaces[0].tabs()[0].root_pane();
    let target_tab = app.state.workspaces[0].test_add_tab(Some("target"));
    let target = app.state.workspaces[0].tabs()[target_tab].root_pane();
    app.state.workspaces[0].set_tab_zoomed(target_tab, true);
    seed_terminal_states(&mut app);
    let source_public = app.public_pane_id(0, source).expect("test precondition");
    let target_tab_public = app.public_tab_id(0, target_tab).expect("test precondition");
    let target_public = app.public_pane_id(0, target).expect("test precondition");

    let response = app.handle_pane_move(PaneMoveParams {
        pane_id: source_public,
        destination: PaneMoveDestination::Tab {
            tab_id: target_tab_public.clone(),
            target_pane_id: Some(target_public),
            split: SplitDirection::Right,
            ratio: None,
        },
        focus: true,
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneMove { move_result } = success.result else {
        panic!("expected pane move response");
    };
    assert!(!move_result.changed);
    assert_eq!(move_result.reason, Some(PaneMoveReason::ZoomedTab));
    assert_eq!(move_result.target_layout.tab_id, target_tab_public);
    assert_eq!(
        move_result
            .source_layout
            .as_ref()
            .map(|layout| layout.tab_id.as_str()),
        app.public_tab_id(0, 0).as_deref()
    );
}

#[test]
fn api_pane_zoom_current_toggles_zoom() {
    let mut app = app_with_workspace();
    app.state.set_active_index(Some(0));
    app.state.set_selected_index(Some(0));
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let _right = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(0, root);
    let root_public = app.public_pane_id(0, root).expect("test precondition");

    let response = app.handle_pane_zoom(&PaneZoomParams::default());

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneZoom { zoom } = success.result else {
        panic!("expected pane zoom response");
    };
    assert!(zoom.changed);
    assert!(zoom.zoom_changed);
    assert!(!zoom.focus_changed);
    assert_eq!(zoom.reason, None);
    assert_eq!(zoom.pane_id, root_public);
    assert_eq!(zoom.focused_pane_id, zoom.pane_id);
    assert!(zoom.zoomed);
    assert!(zoom.layout.zoomed);
    assert!(matches!(
        &app.event_hub.events_after(0).last().expect("layout event").1.data,
        EventData::LayoutUpdated { layout }
            if layout.tab_id == app.public_tab_id(0, 0).expect("test precondition") && layout.zoomed
    ));

    let response = app.handle_pane_zoom(&PaneZoomParams::default());
    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneZoom { zoom } = success.result else {
        panic!("expected pane zoom response");
    };
    assert!(zoom.changed);
    assert!(zoom.zoom_changed);
    assert!(!zoom.focus_changed);
    assert!(!zoom.zoomed);
    assert!(!zoom.layout.zoomed);
    assert!(matches!(
        &app.event_hub.events_after(0).last().expect("layout event").1.data,
        EventData::LayoutUpdated { layout }
            if layout.tab_id == app.public_tab_id(0, 0).expect("test precondition") && !layout.zoomed
    ));
}

#[test]
fn api_pane_zoom_single_pane_returns_noop() {
    let mut app = app_with_workspace();
    app.state.set_active_index(Some(0));
    app.state.set_selected_index(Some(0));
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let root_public = app.public_pane_id(0, root).expect("test precondition");

    let response = app.handle_pane_zoom(&PaneZoomParams {
        pane_id: Some(root_public.clone()),
        mode: PaneZoomMode::Toggle,
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneZoom { zoom } = success.result else {
        panic!("expected pane zoom response");
    };
    assert!(!zoom.changed);
    assert!(!zoom.zoom_changed);
    assert!(!zoom.focus_changed);
    assert_eq!(zoom.reason, Some(PaneZoomReason::SinglePane));
    assert_eq!(zoom.pane_id, root_public);
    assert!(!zoom.zoomed);
    assert!(!app.state.workspaces[0].tabs()[0].zoomed());
}

#[test]
fn api_pane_zoom_on_and_off_are_idempotent() {
    let mut app = app_with_workspace();
    app.state.set_active_index(Some(0));
    app.state.set_selected_index(Some(0));
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let _right = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(0, root);
    let root_public = app.public_pane_id(0, root).expect("test precondition");

    let response = app.handle_pane_zoom(&PaneZoomParams {
        pane_id: Some(root_public.clone()),
        mode: PaneZoomMode::On,
    });
    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneZoom { zoom } = success.result else {
        panic!("expected pane zoom response");
    };
    assert!(zoom.changed);
    assert!(zoom.zoom_changed);
    assert!(!zoom.focus_changed);
    assert!(zoom.zoomed);

    let response = app.handle_pane_zoom(&PaneZoomParams {
        pane_id: Some(root_public.clone()),
        mode: PaneZoomMode::On,
    });
    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneZoom { zoom } = success.result else {
        panic!("expected pane zoom response");
    };
    assert!(!zoom.changed);
    assert!(!zoom.zoom_changed);
    assert!(!zoom.focus_changed);
    assert_eq!(zoom.reason, Some(PaneZoomReason::AlreadyZoomed));
    assert!(zoom.zoomed);

    let response = app.handle_pane_zoom(&PaneZoomParams {
        pane_id: Some(root_public),
        mode: PaneZoomMode::Off,
    });
    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneZoom { zoom } = success.result else {
        panic!("expected pane zoom response");
    };
    assert!(zoom.changed);
    assert!(zoom.zoom_changed);
    assert!(!zoom.focus_changed);
    assert!(!zoom.zoomed);

    let response = app.handle_pane_zoom(&PaneZoomParams {
        pane_id: None,
        mode: PaneZoomMode::Off,
    });
    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneZoom { zoom } = success.result else {
        panic!("expected pane zoom response");
    };
    assert!(!zoom.changed);
    assert!(!zoom.zoom_changed);
    assert!(!zoom.focus_changed);
    assert_eq!(zoom.reason, Some(PaneZoomReason::AlreadyUnzoomed));
    assert!(!zoom.zoomed);
}

#[test]
fn api_pane_zoom_idempotent_mode_reports_focus_change() {
    let mut app = app_with_workspace();
    app.state.set_active_index(Some(0));
    app.state.set_selected_index(Some(0));
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let right = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(0, root);
    app.state.workspaces[0].set_tab_zoomed(0, true);
    let right_public = app.public_pane_id(0, right).expect("test precondition");

    let response = app.handle_pane_zoom(&PaneZoomParams {
        pane_id: Some(right_public),
        mode: PaneZoomMode::On,
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneZoom { zoom } = success.result else {
        panic!("expected pane zoom response");
    };
    assert!(zoom.changed);
    assert!(!zoom.zoom_changed);
    assert!(zoom.focus_changed);
    assert_eq!(zoom.reason, Some(PaneZoomReason::AlreadyZoomed));
    assert!(zoom.zoomed);
    assert_eq!(app.state.workspaces[0].focused_pane_id(), right);
    assert!(matches!(
        &app.event_hub.events_after(0).last().expect("layout event").1.data,
        EventData::LayoutUpdated { layout }
            if layout.focused_pane_id == app.public_pane_id(0, right).expect("test precondition")
    ));
}

#[test]
fn api_pane_zoom_params_serialize_modes() {
    let request = shepr_api::schema::Request {
        id: "req".into(),
        method: shepr_api::schema::Method::PaneZoom(PaneZoomParams {
            pane_id: Some("issue-1".into()),
            mode: PaneZoomMode::On,
        }),
    };

    let encoded = serde_json::to_string(&request).expect("test precondition");
    assert!(encoded.contains("\"method\":\"pane.zoom\""));
    assert!(encoded.contains("\"mode\":\"on\""));

    let decoded: shepr_api::schema::Request =
        serde_json::from_str(&crate::test_support::test_json(&encoded)).expect("test precondition");
    let shepr_api::schema::Method::PaneZoom(params) = decoded.method else {
        panic!("expected pane zoom request");
    };
    assert_eq!(params.pane_id, Some("issue-1".into()));
    assert_eq!(params.mode, PaneZoomMode::On);
}

#[test]
fn api_pane_layout_of_a_zoomed_tab_reports_only_the_zoomed_pane() {
    let mut app = app_with_workspace();
    app.state.set_active_index(Some(0));
    app.state.set_selected_index(Some(0));
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let right = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(0, right);
    app.state.workspaces[0].set_tab_zoomed(0, true);
    app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 20);
    let right_public = app.public_pane_id(0, right).expect("test precondition");
    assert!(app.public_pane_id(0, root).is_some());

    let layout = app.pane_layout_snapshot(0, 0).expect("layout");

    assert!(layout.zoomed);
    assert_eq!(layout.panes.len(), 1);
    assert_eq!(layout.panes[0].pane_id, right_public);
    assert!(layout.panes[0].focused);
    assert_eq!(layout.panes[0].rect, layout.area);
    assert!(layout.splits.is_empty());
}

#[test]
fn api_pane_layout_returns_public_ids_rects_and_splits() {
    let mut app = app_with_workspace();
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let right = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(0, root);
    app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 20);
    let root_public = app.public_pane_id(0, root).expect("test precondition");
    let right_public = app.public_pane_id(0, right).expect("test precondition");

    let response = app.handle_pane_layout(&shepr_api::schema::PaneLayoutParams {
        pane_id: Some(root_public.clone()),
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneLayout { layout } = success.result else {
        panic!("expected pane layout response");
    };
    assert_eq!(layout.focused_pane_id, root_public);
    assert!(layout.panes.iter().any(|pane| pane.pane_id == root_public));
    assert!(layout.panes.iter().any(|pane| pane.pane_id == right_public));
    assert_eq!(layout.splits.len(), 1);
    assert_eq!(
        layout.splits[0].direction,
        shepr_api::schema::SplitDirection::Right
    );
}

#[test]
fn api_pane_neighbor_returns_directional_neighbor_public_id() {
    let mut app = app_with_workspace();
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let right = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(0, root);
    app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 20);
    let root_public = app.public_pane_id(0, root).expect("test precondition");
    let right_public = app.public_pane_id(0, right).expect("test precondition");

    let response = app.handle_pane_neighbor(&shepr_api::schema::PaneNeighborParams {
        pane_id: Some(root_public.clone()),
        direction: PaneDirection::Right,
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneNeighbor { neighbor } = success.result else {
        panic!("expected pane neighbor response");
    };
    assert_eq!(neighbor.pane_id, root_public);
    assert_eq!(neighbor.direction, PaneDirection::Right);
    assert_eq!(neighbor.neighbor_pane_id, Some(right_public));
}

#[test]
fn api_pane_edges_reports_physical_layout_edges() {
    let mut app = app_with_workspace();
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let right = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(0, root);
    app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 20);
    let right_public = app.public_pane_id(0, right).expect("test precondition");

    let response = app.handle_pane_edges(&shepr_api::schema::PaneEdgesParams {
        pane_id: Some(right_public.clone()),
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneEdges { edges } = success.result else {
        panic!("expected pane edges response");
    };
    assert_eq!(edges.pane_id, right_public);
    assert!(!edges.left);
    assert!(edges.right);
    assert!(edges.up);
    assert!(edges.down);
}

#[test]
fn api_pane_resize_changes_target_ratio_without_changing_focus() {
    let mut app = app_with_workspace();
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let right = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(0, right);
    app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 20);
    let root_public = app.public_pane_id(0, root).expect("test precondition");
    let right_public = app.public_pane_id(0, right).expect("test precondition");

    let response = app.handle_pane_resize(&shepr_api::schema::PaneResizeParams {
        pane_id: Some(root_public.clone()),
        direction: PaneDirection::Right,
        amount: Some(0.1),
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneResize { resize } = success.result else {
        panic!("expected pane resize response");
    };
    assert!(resize.changed);
    assert_eq!(resize.reason, None);
    assert_eq!(resize.pane_id, root_public);
    assert_eq!(resize.focused_pane_id, right_public);
    assert_eq!(resize.layout.focused_pane_id, right_public);
    assert!((resize.layout.splits[0].ratio - 0.6).abs() < f32::EPSILON);
    assert_eq!(app.state.workspaces[0].focused_pane_id(), right);
    assert!(matches!(
        &app.event_hub.events_after(0).last().expect("layout event").1.data,
        EventData::LayoutUpdated { layout }
            if layout.tab_id == app.public_tab_id(0, 0).expect("test precondition")
                && (layout.splits[0].ratio - 0.6).abs() < f32::EPSILON
    ));
}

#[test]
fn api_pane_focus_direction_focuses_neighbor() {
    let mut app = app_with_workspace();
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let right = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(0, root);
    app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 20);
    let root_public = app.public_pane_id(0, root).expect("test precondition");
    let right_public = app.public_pane_id(0, right).expect("test precondition");

    let response = app.handle_pane_focus_direction(&shepr_api::schema::PaneFocusDirectionParams {
        pane_id: Some(root_public.clone()),
        direction: PaneDirection::Right,
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneFocusDirection { focus } = success.result else {
        panic!("expected pane focus direction response");
    };
    assert!(focus.changed);
    assert_eq!(focus.reason, None);
    assert_eq!(focus.source_pane_id, root_public);
    assert_eq!(focus.focused_pane_id, Some(right_public.clone()));
    assert_eq!(focus.layout.focused_pane_id, right_public);
    assert_eq!(app.state.workspaces[0].focused_pane_id(), right);
}

#[test]
fn api_pane_focus_focuses_direct_target_across_tabs_and_workspaces() {
    let mut app = app_with_workspace();
    app.state.workspaces.push(Workspace::test_new("other"));
    let target_tab_idx = app.state.workspaces[1].test_add_tab(Some("target"));
    app.state.workspaces[1].switch_tab(target_tab_idx);
    let target_pane = app.state.workspaces[1].tabs()[target_tab_idx].root_pane();
    app.state.ensure_test_terminals();
    let target_public = app
        .public_pane_id(1, target_pane)
        .expect("test precondition");
    app.state.switch_workspace(0);
    assert_eq!(app.state.active_index(), Some(0));

    let response = app.handle_pane_focus(&shepr_api::schema::PaneTarget {
        pane_id: target_public.clone(),
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneInfo { pane } = success.result else {
        panic!("expected pane info response");
    };
    assert_eq!(pane.pane_id, target_public);
    assert_eq!(app.state.active_index(), Some(1));
    assert_eq!(app.state.workspaces[1].active_tab_index(), target_tab_idx);
    assert_eq!(app.state.workspaces[1].focused_pane_id(), target_pane);
    assert_eq!(app.state.mode, Mode::Terminal);
}

#[test]
fn api_pane_focus_returns_idle_agent_status() {
    let mut app = app_with_workspace();
    app.state.set_active_index(Some(0));
    app.state.set_selected_index(Some(0));

    let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
    let terminal_id = app.state.workspaces[0].tabs()[0].panes()[&pane_id]
        .attached_terminal_id
        .clone();
    app.state
        .terminals
        .get_mut(&terminal_id)
        .expect("test precondition")
        .state = shepr_agent::detect::AgentState::Idle;
    app.state.workspaces[0].focus_pane_in_tab(0, pane_id);

    let public_pane_id = app.public_pane_id(0, pane_id).expect("test precondition");
    let response = app.handle_pane_focus(&PaneTarget {
        pane_id: public_pane_id,
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneInfo { pane } = success.result else {
        panic!("expected pane info response");
    };
    assert_eq!(pane.agent_status, shepr_api::schema::AgentStatus::Idle);
}

#[test]
fn api_pane_focus_rejects_invalid_pane_id() {
    let mut app = app_with_workspace();

    let response = app.handle_pane_focus(&shepr_api::schema::PaneTarget {
        pane_id: "pane_missing".into(),
    });

    let error: ErrorResponse = crate::test_support::test_error(&response);
    assert_eq!(error.error.code, "pane_not_found");
}

#[test]
fn api_pane_focus_direction_no_neighbor_is_noop() {
    let mut app = app_with_workspace();
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    app.state.workspaces[0].focus_pane_in_tab(0, root);
    app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 20);
    let root_public = app.public_pane_id(0, root).expect("test precondition");

    let response = app.handle_pane_focus_direction(&shepr_api::schema::PaneFocusDirectionParams {
        pane_id: Some(root_public.clone()),
        direction: PaneDirection::Left,
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneFocusDirection { focus } = success.result else {
        panic!("expected pane focus direction response");
    };
    assert!(!focus.changed);
    assert_eq!(focus.reason, Some(PaneFocusDirectionReason::NoNeighbor));
    assert_eq!(focus.source_pane_id, root_public.clone());
    assert_eq!(focus.focused_pane_id, Some(root_public));
    assert_eq!(app.state.workspaces[0].focused_pane_id(), root);
}

#[test]
fn pane_metadata_tokens_patch_and_clear_through_dispatcher() {
    let (mut app, pane_id) = app_with_test_workspace();
    for (tokens, expected) in [
        (
            std::collections::HashMap::from([
                ("summary".into(), Some("reviewing auth".into())),
                ("model".into(), Some("opus".into())),
            ]),
            std::collections::HashMap::from([
                ("summary".into(), "reviewing auth".into()),
                ("model".into(), "opus".into()),
            ]),
        ),
        (
            std::collections::HashMap::from([
                ("summary".into(), Some("done".into())),
                ("model".into(), None),
            ]),
            std::collections::HashMap::from([("summary".into(), "done".into())]),
        ),
    ] {
        let mut params = metadata_params(pane_id.clone());
        params.title = None;
        params.tokens = tokens;
        let response = app.handle_api_request(shepr_api::schema::Request {
            id: "set".into(),
            method: shepr_api::schema::Method::PaneReportMetadata(params),
        });
        let success: SuccessResponse = crate::test_support::test_success(&response);
        assert_eq!(success.result, ResponseResult::Ok {});

        let response = app.handle_api_request(shepr_api::schema::Request {
            id: "get".into(),
            method: shepr_api::schema::Method::PaneGet(PaneTarget {
                pane_id: pane_id.clone(),
            }),
        });
        let success: SuccessResponse = crate::test_support::test_success(&response);
        let ResponseResult::PaneInfo { pane } = success.result else {
            panic!("expected pane info");
        };
        assert_eq!(pane.tokens, expected);
    }
    let (_, internal_pane_id) = app.parse_pane_id(&pane_id).expect("test precondition");
    let terminal_id = app.state.workspaces[0]
        .pane_state(internal_pane_id)
        .expect("test precondition")
        .attached_terminal_id
        .clone();
    assert!(app.state.terminals[&terminal_id].agent_metadata.is_empty());
}

#[test]
fn pane_tokens_are_independent_from_presentation_guards() {
    let (mut app, pane_id) = app_with_test_workspace();
    let (_, internal_pane_id) = app.parse_pane_id(&pane_id).expect("test precondition");
    let terminal_id = app.state.workspaces[0]
        .pane_state(internal_pane_id)
        .expect("test precondition")
        .attached_terminal_id
        .clone();
    app.state
        .terminals
        .get_mut(&terminal_id)
        .expect("test precondition")
        .set_detected_state(Some(Agent::Claude), AgentState::Working);
    let mut params = metadata_params(pane_id);
    params.title = None;
    params.agent = Some("codex".into());
    params.tokens = std::collections::HashMap::from([("summary".into(), Some("global".into()))]);

    let response = app.handle_pane_report_metadata(params);

    let _: SuccessResponse = crate::test_support::test_success(&response);
    assert_eq!(
        app.state.terminals[&terminal_id].metadata_tokens.values(),
        std::collections::HashMap::from([("summary".into(), "global".into())])
    );
}

#[test]
fn pane_metadata_uses_one_sequence_for_presentation_and_tokens() {
    let (mut app, pane_id) = app_with_test_workspace();
    let mut presentation = metadata_params(pane_id.clone());
    presentation.seq = Some(10);
    let response = app.handle_pane_report_metadata(presentation);
    let _: SuccessResponse = crate::test_support::test_success(&response);

    let mut stale_token = metadata_params(pane_id.clone());
    stale_token.title = None;
    stale_token.tokens =
        std::collections::HashMap::from([("summary".into(), Some("stale".into()))]);
    stale_token.seq = Some(9);
    let response = app.handle_pane_report_metadata(stale_token);
    let _: SuccessResponse = crate::test_support::test_success(&response);

    let (_, internal_pane_id) = app.parse_pane_id(&pane_id).expect("test precondition");
    let terminal_id = app.state.workspaces[0]
        .pane_state(internal_pane_id)
        .expect("test precondition")
        .attached_terminal_id
        .clone();
    assert!(
        app.state.terminals[&terminal_id]
            .metadata_tokens
            .values()
            .is_empty()
    );
}

#[test]
fn pane_metadata_ignored_after_process_exit_does_not_poison_sequence() {
    let (mut app, pane_id) = app_with_test_workspace();
    let (_, internal_pane_id) = app.parse_pane_id(&pane_id).expect("test precondition");
    let terminal_id = app.state.workspaces[0]
        .pane_state(internal_pane_id)
        .expect("test precondition")
        .attached_terminal_id
        .clone();
    app.state
        .terminals
        .get_mut(&terminal_id)
        .expect("test precondition")
        .set_detected_state(Some(Agent::Pi), AgentState::Idle);

    let mut initial = metadata_params(pane_id.clone());
    initial.source = "custom:pi-metadata".into();
    initial.agent = Some("pi".into());
    initial.seq = Some(100);
    let response = app.handle_pane_report_metadata(initial);
    let _: SuccessResponse = crate::test_support::test_success(&response);

    let mut initial_tokens = metadata_params(pane_id.clone());
    initial_tokens.source = "custom:pi-tokens".into();
    initial_tokens.agent = Some("pi".into());
    initial_tokens.title = None;
    initial_tokens.tokens =
        std::collections::HashMap::from([("generation".into(), Some("old".into()))]);
    initial_tokens.seq = Some(100);
    let response = app.handle_pane_report_metadata(initial_tokens);
    let _: SuccessResponse = crate::test_support::test_success(&response);

    let exit_at = std::time::Instant::now() + std::time::Duration::from_millis(1);
    app.state
        .terminals
        .get_mut(&terminal_id)
        .expect("test precondition")
        .set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            true,
            exit_at,
        );
    app.state
        .terminals
        .get_mut(&terminal_id)
        .expect("test precondition")
        .set_detected_state_with_screen_signals_at(
            None,
            AgentState::Unknown,
            false,
            false,
            exit_at + std::time::Duration::from_millis(1),
        );

    let mut stale = metadata_params(pane_id.clone());
    stale.source = "custom:pi-metadata".into();
    stale.agent = Some("pi".into());
    stale.title = Some("stale".into());
    stale.seq = Some(200);
    let response = app.handle_pane_report_metadata(stale);
    let _: SuccessResponse = crate::test_support::test_success(&response);

    let mut official = metadata_params(pane_id.clone());
    official.source = "shepr:pi".into();
    official.seq = Some(200);
    let response = app.handle_pane_report_metadata(official);
    let _: SuccessResponse = crate::test_support::test_success(&response);

    let terminal = &app.state.terminals[&terminal_id];
    assert!(terminal.metadata_report_sequence_is_fresh("custom:pi-metadata", Some(1)));
    assert!(terminal.metadata_report_sequence_is_fresh("custom:pi-tokens", Some(1)));
    assert!(terminal.metadata_report_sequence_is_fresh("shepr:pi", Some(1)));

    app.state
        .terminals
        .get_mut(&terminal_id)
        .expect("test precondition")
        .set_detected_state_with_screen_signals_at(
            Some(Agent::Pi),
            AgentState::Idle,
            false,
            false,
            exit_at + std::time::Duration::from_millis(2),
        );
    let mut fresh = metadata_params(pane_id.clone());
    fresh.source = "custom:pi-metadata".into();
    fresh.agent = Some("pi".into());
    fresh.title = Some("fresh".into());
    fresh.seq = Some(1);
    let response = app.handle_pane_report_metadata(fresh);
    let _: SuccessResponse = crate::test_support::test_success(&response);

    let mut fresh_tokens = metadata_params(pane_id);
    fresh_tokens.source = "custom:pi-tokens".into();
    fresh_tokens.agent = Some("pi".into());
    fresh_tokens.title = None;
    fresh_tokens.tokens =
        std::collections::HashMap::from([("generation".into(), Some("new".into()))]);
    fresh_tokens.seq = Some(1);
    let response = app.handle_pane_report_metadata(fresh_tokens);
    let _: SuccessResponse = crate::test_support::test_success(&response);

    let terminal = &app.state.terminals[&terminal_id];
    assert_eq!(
        terminal.agent_metadata["custom:pi-metadata"]
            .title
            .as_deref(),
        Some("fresh")
    );
    assert_eq!(
        terminal
            .metadata_tokens
            .values()
            .get("generation")
            .map(String::as_str),
        Some("new")
    );
}

#[test]
fn pane_report_metadata_accepts_documented_source_chars_and_max_ttl() {
    let (mut app, pane_id) = app_with_test_workspace();
    let mut params = metadata_params(pane_id);
    params.ttl_ms = Some(METADATA_TTL_MAX_MS);

    let response = app.handle_pane_report_metadata(params);

    let _: SuccessResponse = crate::test_support::test_success(&response);
}

#[test]
fn pane_report_metadata_rejects_invalid_source_shape() {
    let (mut app, pane_id) = app_with_test_workspace();
    for source in ["", "user metadata", "user/metadata", "user:\u{7f}metadata"] {
        let mut params = metadata_params(pane_id.clone());
        params.source = source.into();

        let response = app.handle_pane_report_metadata(params);

        assert_eq!(
            metadata_error_code(&response),
            ApiErrorCode::InvalidMetadataSource
        );
    }
}

#[test]
fn pane_report_metadata_rejects_long_source() {
    let (mut app, pane_id) = app_with_test_workspace();
    let mut params = metadata_params(pane_id);
    params.source = "a".repeat(METADATA_SOURCE_MAX_CHARS + 1);

    let response = app.handle_pane_report_metadata(params);

    assert_eq!(
        metadata_error_code(&response),
        ApiErrorCode::InvalidMetadataSource
    );
}

#[test]
fn pane_report_metadata_rejects_invalid_applies_to_source() {
    let (mut app, pane_id) = app_with_test_workspace();
    let mut params = metadata_params(pane_id);
    params.applies_to_source = Some("shepr source".into());

    let response = app.handle_pane_report_metadata(params);

    assert_eq!(
        metadata_error_code(&response),
        ApiErrorCode::InvalidMetadataSource
    );
}

#[test]
fn pane_report_metadata_rejects_ttl_outside_supported_range() {
    let (mut app, pane_id) = app_with_test_workspace();
    for ttl_ms in [0, METADATA_TTL_MAX_MS + 1] {
        let mut params = metadata_params(pane_id.clone());
        params.ttl_ms = Some(ttl_ms);

        let response = app.handle_pane_report_metadata(params);

        assert_eq!(
            metadata_error_code(&response),
            ApiErrorCode::InvalidMetadataTtl
        );
    }
}
