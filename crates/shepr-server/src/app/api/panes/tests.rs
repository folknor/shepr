use super::*;
use crate::app::Mode;
use crate::test_support::*;
use shepr_api::schema::{ErrorResponse, SuccessResponse};
use shepr_config::Config;
use shepr_mux::workspace::Workspace;

fn app_with_test_workspace() -> (App, String) {
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(&Config::default(), crate::app::AppPolicy::Test, api_rx);
    app.state.workspaces = vec![Workspace::test_new("metadata")];
    app.state.ensure_test_terminals();
    let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
    let public_pane_id = app.public_pane_id(0, pane_id).expect("test precondition");
    (app, public_pane_id.to_string())
}

#[test]
fn pane_input_set_changes_only_the_target_pane() {
    let (mut app, public_pane_id) = app_with_test_workspace();
    let target = app.state.workspaces[0].tabs()[0].root_pane();
    let other = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);

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
async fn pane_info_exposes_scroll_metrics() {
    let (app, _public_pane_id, pane_id) = app_with_scrollback_runtime();
    let runtime = app
        .state
        .runtime_for_pane_in_workspace(&app.terminal_runtimes, 0, pane_id)
        .expect("runtime");
    runtime.scroll_up(3);

    let pane = app.pane_info(0, pane_id).expect("pane info");
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
    runtime.test_process_pty_bytes(b"\r\nagent is still working");
    let params = PaneSelectionReadParams {
        pane_id: public_pane_id.clone(),
        anchor: PaneTextPoint {
            row: shepr_vt::AbsRow(0),
            col: 0,
        },
        cursor: PaneTextPoint {
            row: shepr_vt::AbsRow(0),
            col: 4,
        },
    };
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
        cursor: PaneTextPoint {
            row: shepr_vt::AbsRow(0),
            col: 0,
        },
        motion: PaneCopyMotion::NextWordStart,
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    assert_eq!(
        success.result,
        ResponseResult::PaneCopyMotion {
            pane_id: public_pane_id,
            cursor: PaneTextPoint {
                row: shepr_vt::AbsRow(0),
                col: 6
            },
        }
    );
}

/// Output that evicts history between two requests does not move the line a
/// copy-mode point names: motions and searches read absolute rows.
#[tokio::test]
async fn api_copy_motion_and_search_keep_their_line_across_eviction() {
    let (mut app, public_pane_id) = app_with_test_workspace();
    let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
    let initial: String = (0..1_100).map(|i| format!("{i:06}\r\n")).collect();
    // One byte of scrollback budget buys the minimum history, so every
    // further line evicts one.
    app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_scrollback_bytes(10, 3, 1, initial.as_bytes()),
    );
    let search = |app: &mut App| {
        let response = app.handle_pane_copy_search(PaneCopySearchParams {
            pane_id: public_pane_id.clone(),
            query: "001099".into(),
            direction: PaneCopySearchDirection::Forward,
            cursor: PaneTextPoint {
                row: shepr_vt::AbsRow(0),
                col: 0,
            },
            previous: None,
        });
        let success: SuccessResponse = crate::test_support::test_success(&response);
        let ResponseResult::PaneCopySearch { matches, .. } = success.result else {
            panic!("expected copy search response");
        };
        matches
    };
    let found = search(&mut app);
    assert_eq!(found.len(), 1);
    let line = found[0].start;
    assert_eq!(line.row, shepr_vt::AbsRow(1_099));

    let runtime = app
        .state
        .runtime_for_pane_in_workspace(&app.terminal_runtimes, 0, pane_id)
        .expect("test precondition");
    let origin_before = runtime
        .scroll_metrics()
        .expect("test precondition")
        .history_origin;
    runtime.test_process_pty_bytes(b"x\r\n");
    assert!(
        runtime
            .scroll_metrics()
            .expect("test precondition")
            .history_origin
            > origin_before,
        "the output must evict history"
    );

    assert_eq!(search(&mut app)[0].start, line);
    let response = app.handle_pane_copy_motion(PaneCopyMotionParams {
        pane_id: public_pane_id.clone(),
        cursor: line,
        motion: PaneCopyMotion::NextWordEnd,
    });
    let success: SuccessResponse = crate::test_support::test_success(&response);
    assert_eq!(
        success.result,
        ResponseResult::PaneCopyMotion {
            pane_id: public_pane_id,
            cursor: PaneTextPoint {
                row: line.row,
                col: 5
            },
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
            row: shepr_vt::AbsRow(0),
            col: 2,
        },
        motion: PaneCopyMotion::NextParagraph,
    });
    let success: SuccessResponse = crate::test_support::test_success(&response);
    assert_eq!(
        success.result,
        ResponseResult::PaneCopyMotion {
            pane_id: public_pane_id,
            cursor: PaneTextPoint {
                row: shepr_vt::AbsRow(1),
                col: 2
            },
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

    let response = app.handle_pane_copy_search(PaneCopySearchParams {
        pane_id: public_pane_id.clone(),
        query: "alpha".into(),
        direction: PaneCopySearchDirection::Forward,
        cursor: PaneTextPoint {
            row: shepr_vt::AbsRow(0),
            col: 0,
        },
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
            row: shepr_vt::AbsRow(0),
            col: 0
        }
    );
    assert_eq!(
        matches[1].start,
        PaneTextPoint {
            row: shepr_vt::AbsRow(0),
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
    let response = app.handle_pane_copy_search(PaneCopySearchParams {
        pane_id: public_pane_id,
        query: "a".into(),
        direction: PaneCopySearchDirection::Forward,
        cursor: PaneTextPoint {
            row: shepr_vt::AbsRow(0),
            col: 0,
        },
        previous: None,
    });
    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneCopySearch { matches, total, .. } = success.result else {
        panic!("expected copy search response");
    };
    assert_eq!(total, 1500);
    assert_eq!(matches.len(), 1024);
}

#[test]
fn api_pane_rename_returns_the_renamed_pane() {
    let (mut app, public_pane_id) = app_with_test_workspace();

    let response = app.handle_pane_rename(PaneRenameParams {
        pane_id: public_pane_id.clone(),
        label: Some("build".into()),
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneInfo { pane } = success.result else {
        panic!("expected pane info response");
    };
    assert_eq!(pane.pane_id, public_pane_id);
    assert_eq!(pane.label.as_deref(), Some("build"));
}

fn app_with_workspace() -> App {
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(&Config::default(), crate::app::AppPolicy::Test, api_rx);
    app.state.workspaces = vec![Workspace::test_new("issue")];
    app.state.ensure_test_terminals();
    app
}

#[test]
fn api_pane_close_of_last_pane_closes_workspace() {
    let mut app = app_with_workspace();
    let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
    let public_pane_id = app.public_pane_id(0, pane_id).expect("test precondition");

    let response = app.handle_pane_close(&PaneTarget {
        pane_id: public_pane_id.to_string(),
    });

    let _: SuccessResponse = crate::test_support::test_success(&response);
    assert!(app.state.workspaces.is_empty());
}

#[test]
fn api_pane_close_of_a_tabs_last_pane_closes_the_tab() {
    let mut app = app_with_workspace();
    app.state.workspaces[0].test_add_tab(Some("survivor"));
    app.state.ensure_test_terminals();
    let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
    let survivor_root = app.state.workspaces[0].tabs()[1].root_pane();
    let public_pane_id = app.public_pane_id(0, pane_id).expect("test precondition");

    let response = app.handle_pane_close(&PaneTarget {
        pane_id: public_pane_id.to_string(),
    });

    let _: SuccessResponse = crate::test_support::test_success(&response);
    assert_eq!(app.state.workspaces[0].tabs().len(), 1);
    assert_eq!(app.state.workspaces[0].tabs()[0].root_pane(), survivor_root);
}

#[test]
fn api_pane_swap_explicit_source_and_target_preserves_focus_and_returns_layout() {
    let mut app = app_with_workspace();
    let source = app.state.workspaces[0].tabs()[0].root_pane();
    let target = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(0, source);
    app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 20);
    let source_public = app.public_pane_id(0, source).expect("test precondition");
    let target_public = app.public_pane_id(0, target).expect("test precondition");

    let response = app.handle_pane_swap(PaneSwapParams {
        source_pane_id: Some(source_public.clone().to_string()),
        target_pane_id: Some(target_public.clone().to_string()),
        ..PaneSwapParams::default()
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneSwap { swap } = success.result else {
        panic!("expected pane swap response");
    };
    assert!(swap.changed);
    assert_eq!(swap.reason, None);
    assert_eq!(swap.source_pane_id, source_public);
    assert_eq!(
        swap.target_pane_id,
        Some(target_public).map(|id| id.to_string())
    );
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
        pane_id: Some(source_public.clone().to_string()),
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
}

#[test]
fn api_pane_swap_explicit_missing_target_returns_not_found_noop() {
    let mut app = app_with_workspace();
    let source = app.state.workspaces[0].tabs()[0].root_pane();
    let source_public = app.public_pane_id(0, source).expect("test precondition");

    let response = app.handle_pane_swap(PaneSwapParams {
        source_pane_id: Some(source_public.clone().to_string()),
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
        target_pane_id: Some(target_public.clone().to_string()),
        ..PaneSwapParams::default()
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneSwap { swap } = success.result else {
        panic!("expected pane swap response");
    };
    assert!(!swap.changed);
    assert_eq!(swap.reason, Some(PaneSwapReason::NotFound));
    assert_eq!(swap.source_pane_id, "missing-pane");
    assert_eq!(
        swap.target_pane_id,
        Some(target_public).map(|id| id.to_string())
    );
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
        source_pane_id: Some(source_public.clone().to_string()),
        target_pane_id: Some(target_public.clone().to_string()),
        ..PaneSwapParams::default()
    });

    let success: SuccessResponse = crate::test_support::test_success(&response);
    let ResponseResult::PaneSwap { swap } = success.result else {
        panic!("expected pane swap response");
    };
    assert!(!swap.changed);
    assert_eq!(swap.reason, Some(PaneSwapReason::CrossTab));
    assert_eq!(swap.source_pane_id, source_public);
    assert_eq!(
        swap.target_pane_id,
        Some(target_public).map(|id| id.to_string())
    );
    assert_eq!(
        swap.layout.workspace_id,
        app.public_workspace_id(0).expect("test precondition")
    );
}

#[test]
fn api_pane_zoom_current_toggles_zoom() {
    let mut app = app_with_workspace();
    app.state.set_active_index(Some(0));
    app.state.set_selected_index(Some(0));
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let _right = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
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
}

#[test]
fn api_pane_zoom_single_pane_returns_noop() {
    let mut app = app_with_workspace();
    app.state.set_active_index(Some(0));
    app.state.set_selected_index(Some(0));
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let root_public = app.public_pane_id(0, root).expect("test precondition");

    let response = app.handle_pane_zoom(&PaneZoomParams {
        pane_id: Some(root_public.clone().to_string()),
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
    let _right = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(0, root);
    let root_public = app.public_pane_id(0, root).expect("test precondition");

    let response = app.handle_pane_zoom(&PaneZoomParams {
        pane_id: Some(root_public.clone().to_string()),
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
        pane_id: Some(root_public.clone().to_string()),
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
        pane_id: Some(root_public.to_string()),
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
    let right = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(0, root);
    app.state.workspaces[0].set_tab_zoomed(0, true);
    let right_public = app.public_pane_id(0, right).expect("test precondition");

    let response = app.handle_pane_zoom(&PaneZoomParams {
        pane_id: Some(right_public.to_string()),
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
    assert_eq!(zoom.layout.focused_pane_id, right_public);
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
    let right = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
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
fn api_pane_resize_changes_target_ratio_without_changing_focus() {
    let mut app = app_with_workspace();
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let right = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(0, right);
    app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 20);
    let root_public = app.public_pane_id(0, root).expect("test precondition");
    let right_public = app.public_pane_id(0, right).expect("test precondition");

    let response = app.handle_pane_resize(&shepr_api::schema::PaneResizeParams {
        pane_id: Some(root_public.clone().to_string()),
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
}

#[test]
fn api_pane_focus_direction_focuses_neighbor() {
    let mut app = app_with_workspace();
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let right = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(0, root);
    app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 20);
    let root_public = app.public_pane_id(0, root).expect("test precondition");
    let right_public = app.public_pane_id(0, right).expect("test precondition");

    let response = app.handle_pane_focus_direction(&shepr_api::schema::PaneFocusDirectionParams {
        pane_id: Some(root_public.clone().to_string()),
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
        pane_id: target_public.clone().to_string(),
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
        pane_id: public_pane_id.to_string(),
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
        pane_id: Some(root_public.clone().to_string()),
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
