use super::*;
use crate::app::Mode;
use crate::test_support::*;
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
        right_click: shepr_protocol::command::PaneRightClickTarget::Pane,
    });

    assert_eq!(response, Ok(EndpointReply::Done));
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
async fn clear_pane_mutates_endpoint_owned_history() {
    let (mut app, public_pane_id, pane_id) = app_with_scrollback_runtime();
    let command = shepr_protocol::command::EndpointCommand::PaneClear(PaneTarget {
        pane_id: public_pane_id,
    });
    assert!(command.traits().mutates_ui);
    let response = app.handle_endpoint_command(command);
    assert_eq!(response, Ok(EndpointReply::Done));
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
async fn pane_scroll_sets_and_clamps_endpoint_owned_history() {
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

    let Ok(EndpointReply::PaneInfo { pane }) = response else {
        panic!("expected pane info, got {response:?}");
    };
    assert_eq!(
        pane.scroll.expect("scroll metrics").offset_from_bottom,
        max_offset as u64
    );
}

#[tokio::test]
async fn pane_selection_read_uses_endpoint_terminal_text() {
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

    assert_eq!(
        response,
        Ok(EndpointReply::PaneSelection {
            pane_id: public_pane_id,
            text: "hello".into(),
        })
    );
}

#[tokio::test]
async fn copy_motion_uses_endpoint_terminal_word_semantics() {
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

    assert_eq!(
        response,
        Ok(EndpointReply::PaneCopyMotion {
            pane_id: public_pane_id,
            cursor: PaneTextPoint {
                row: shepr_vt::AbsRow(0),
                col: 6
            },
        })
    );
}

/// Output that evicts history between two requests does not move the line a
/// copy-mode point names: motions and searches read absolute rows.
#[tokio::test]
async fn copy_motion_and_search_keep_their_line_across_eviction() {
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
        let Ok(EndpointReply::PaneCopySearch { matches, .. }) = response else {
            panic!("expected copy search, got {response:?}");
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
    assert_eq!(
        response,
        Ok(EndpointReply::PaneCopyMotion {
            pane_id: public_pane_id,
            cursor: PaneTextPoint {
                row: line.row,
                col: 5
            },
        })
    );
}

#[tokio::test]
async fn paragraph_motion_preserves_the_copy_cursor_column() {
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
    assert_eq!(
        response,
        Ok(EndpointReply::PaneCopyMotion {
            pane_id: public_pane_id,
            cursor: PaneTextPoint {
                row: shepr_vt::AbsRow(1),
                col: 2
            },
        })
    );
}

#[tokio::test]
async fn copy_search_uses_endpoint_terminal_matches_and_wraps() {
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

    let Ok(EndpointReply::PaneCopySearch {
        pane_id,
        matches,
        current,
        total,
        current_global,
    }) = response
    else {
        panic!("expected copy search, got {response:?}");
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
async fn copy_search_bounds_returned_matches_but_keeps_exact_total() {
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
    let Ok(EndpointReply::PaneCopySearch { matches, total, .. }) = response else {
        panic!("expected copy search, got {response:?}");
    };
    assert_eq!(total, 1500);
    assert_eq!(matches.len(), 1024);
}

#[test]
fn pane_rename_returns_the_renamed_pane() {
    let (mut app, public_pane_id) = app_with_test_workspace();

    let response = app.handle_pane_rename(PaneRenameParams {
        pane_id: public_pane_id.clone(),
        label: Some("build".into()),
    });

    let Ok(EndpointReply::PaneInfo { pane }) = response else {
        panic!("expected pane info, got {response:?}");
    };
    assert_eq!(pane.pane_id, public_pane_id);
    assert_eq!(pane.label.as_deref(), Some("build"));
}

#[test]
fn pane_rename_sets_and_clears_the_manual_label() {
    let (mut app, public_pane_id) = app_with_test_workspace();
    let pane = app.state.workspaces[0].tabs()[0].root_pane();
    let terminal_id = app.state.workspaces[0]
        .pane_state(pane)
        .expect("test precondition")
        .attached_terminal_id
        .clone();

    app.handle_pane_rename(PaneRenameParams {
        pane_id: public_pane_id.clone(),
        label: Some("reviewer".into()),
    })
    .expect("the pane is renamed");
    assert_eq!(
        app.state.terminals[&terminal_id].manual_label.as_deref(),
        Some("reviewer")
    );

    let response = app.handle_pane_rename(PaneRenameParams {
        pane_id: public_pane_id,
        label: None,
    });
    let Ok(EndpointReply::PaneInfo { pane }) = response else {
        panic!("expected pane info, got {response:?}");
    };
    assert!(pane.label.is_none());
    assert!(app.state.terminals[&terminal_id].manual_label.is_none());
}

fn app_with_workspace() -> App {
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(&Config::default(), crate::app::AppPolicy::Test, api_rx);
    app.state.workspaces = vec![Workspace::test_new("issue")];
    app.state.ensure_test_terminals();
    app
}

#[test]
fn pane_close_of_last_pane_closes_workspace() {
    let mut app = app_with_workspace();
    let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
    let public_pane_id = app.public_pane_id(0, pane_id).expect("test precondition");

    let response = app.handle_pane_close(&PaneTarget {
        pane_id: public_pane_id.to_string(),
    });

    assert_eq!(response, Ok(EndpointReply::Done));
    assert!(app.state.workspaces.is_empty());
}

#[test]
fn pane_close_of_a_tabs_last_pane_closes_the_tab() {
    let mut app = app_with_workspace();
    app.state.workspaces[0].test_add_tab(Some("survivor"));
    app.state.ensure_test_terminals();
    let pane_id = app.state.workspaces[0].tabs()[0].root_pane();
    let survivor_root = app.state.workspaces[0].tabs()[1].root_pane();
    let public_pane_id = app.public_pane_id(0, pane_id).expect("test precondition");

    let response = app.handle_pane_close(&PaneTarget {
        pane_id: public_pane_id.to_string(),
    });

    assert_eq!(response, Ok(EndpointReply::Done));
    assert_eq!(app.state.workspaces[0].tabs().len(), 1);
    assert_eq!(app.state.workspaces[0].tabs()[0].root_pane(), survivor_root);
}

/// Lays the first workspace's first tab out in a 100x20 area, as the server
/// does when it applies PTY geometry.
fn lay_out_first_tab(app: &mut App) {
    let workspace = &app.state.workspaces[0];
    let key = crate::app::state::TabAreaKey::new(&workspace.id, workspace.tabs()[0].number());
    app.state
        .record_tab_area(key, ratatui::layout::Rect::new(0, 0, 100, 20));
}

/// The panes of the first tab in layout order.
fn tab_pane_order(app: &App) -> Vec<PaneId> {
    app.state.workspaces[0].tabs()[0].layout().pane_ids()
}

#[test]
fn pane_swap_explicit_source_and_target_swaps_and_keeps_focus_on_the_source() {
    let mut app = app_with_workspace();
    let source = app.state.workspaces[0].tabs()[0].root_pane();
    let target = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(0, source);
    lay_out_first_tab(&mut app);
    let source_public = app.public_pane_id(0, source).expect("test precondition");
    let target_public = app.public_pane_id(0, target).expect("test precondition");
    assert_eq!(tab_pane_order(&app), vec![source, target]);

    let response = app.handle_pane_swap(&PaneSwapParams {
        source_pane_id: Some(source_public.to_string()),
        target_pane_id: Some(target_public.to_string()),
        ..PaneSwapParams::default()
    });

    assert_eq!(response, Ok(EndpointReply::Done));
    assert_eq!(tab_pane_order(&app), vec![target, source]);
    assert_eq!(app.state.workspaces[0].focused_pane_id(), source);
}

#[test]
fn pane_swap_without_a_neighbor_is_a_noop() {
    let mut app = app_with_workspace();
    let source = app.state.workspaces[0].tabs()[0].root_pane();
    app.state.workspaces[0].focus_pane_in_tab(0, source);
    lay_out_first_tab(&mut app);
    let source_public = app.public_pane_id(0, source).expect("test precondition");

    let response = app.handle_pane_swap(&PaneSwapParams {
        pane_id: Some(source_public.to_string()),
        direction: Some(PaneDirection::Left),
        ..PaneSwapParams::default()
    });

    assert_eq!(response, Ok(EndpointReply::Done));
    assert_eq!(tab_pane_order(&app), vec![source]);
}

#[test]
fn pane_swap_with_an_unknown_pane_is_a_noop() {
    for missing_source in [false, true] {
        let mut app = app_with_workspace();
        let root = app.state.workspaces[0].tabs()[0].root_pane();
        let right = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
        lay_out_first_tab(&mut app);
        let root_public = app
            .public_pane_id(0, root)
            .expect("test precondition")
            .to_string();
        let (source, target) = if missing_source {
            ("missing-pane".to_owned(), root_public)
        } else {
            (root_public, "missing-pane".to_owned())
        };

        let response = app.handle_pane_swap(&PaneSwapParams {
            source_pane_id: Some(source),
            target_pane_id: Some(target),
            ..PaneSwapParams::default()
        });

        assert_eq!(response, Ok(EndpointReply::Done), "{missing_source}");
        assert_eq!(tab_pane_order(&app), vec![root, right], "{missing_source}");
    }
}

#[test]
fn pane_swap_across_workspaces_is_a_noop() {
    let mut app = app_with_workspace();
    app.state.workspaces.push(Workspace::test_new("other"));
    let source = app.state.workspaces[0].tabs()[0].root_pane();
    let target = app.state.workspaces[1].tabs()[0].root_pane();
    let source_public = app.public_pane_id(0, source).expect("test precondition");
    let target_public = app.public_pane_id(1, target).expect("test precondition");

    let response = app.handle_pane_swap(&PaneSwapParams {
        source_pane_id: Some(source_public.to_string()),
        target_pane_id: Some(target_public.to_string()),
        ..PaneSwapParams::default()
    });

    assert_eq!(response, Ok(EndpointReply::Done));
    assert_eq!(app.state.workspaces[0].tabs()[0].root_pane(), source);
    assert_eq!(app.state.workspaces[1].tabs()[0].root_pane(), target);
}

#[test]
fn pane_swap_needs_either_a_direction_or_both_panes() {
    let mut app = app_with_workspace();

    let response = app.handle_pane_swap(&PaneSwapParams::default());

    assert_eq!(
        response.expect_err("an empty swap is refused").code,
        ApiErrorCode::InvalidPaneSwap
    );
}

#[test]
fn pane_zoom_current_toggles_zoom() {
    let mut app = app_with_workspace();
    app.state.set_active_index(Some(0));
    app.state.set_selected_index(Some(0));
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let _right = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(0, root);

    let response = app.handle_pane_zoom(&PaneZoomParams::default());

    assert_eq!(response, Ok(EndpointReply::Done));
    assert!(app.state.workspaces[0].tabs()[0].zoomed());
    assert_eq!(app.state.workspaces[0].focused_pane_id(), root);

    let response = app.handle_pane_zoom(&PaneZoomParams::default());
    assert_eq!(response, Ok(EndpointReply::Done));
    assert!(!app.state.workspaces[0].tabs()[0].zoomed());
}

#[test]
fn pane_zoom_of_a_single_pane_is_a_noop() {
    let mut app = app_with_workspace();
    app.state.set_active_index(Some(0));
    app.state.set_selected_index(Some(0));
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let root_public = app.public_pane_id(0, root).expect("test precondition");

    let response = app.handle_pane_zoom(&PaneZoomParams {
        pane_id: Some(root_public.to_string()),
        mode: PaneZoomMode::Toggle,
    });

    assert_eq!(response, Ok(EndpointReply::Done));
    assert!(!app.state.workspaces[0].tabs()[0].zoomed());
}

#[test]
fn pane_zoom_on_and_off_are_idempotent() {
    let mut app = app_with_workspace();
    app.state.set_active_index(Some(0));
    app.state.set_selected_index(Some(0));
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let _right = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(0, root);
    let root_public = app.public_pane_id(0, root).expect("test precondition");

    for (pane_id, mode, zoomed) in [
        (Some(root_public.to_string()), PaneZoomMode::On, true),
        (Some(root_public.to_string()), PaneZoomMode::On, true),
        (Some(root_public.to_string()), PaneZoomMode::Off, false),
        (None, PaneZoomMode::Off, false),
    ] {
        let response = app.handle_pane_zoom(&PaneZoomParams { pane_id, mode });
        assert_eq!(response, Ok(EndpointReply::Done), "{mode:?}");
        assert_eq!(
            app.state.workspaces[0].tabs()[0].zoomed(),
            zoomed,
            "{mode:?}"
        );
    }
}

#[test]
fn pane_zoom_on_an_unfocused_pane_of_a_zoomed_tab_moves_focus() {
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

    assert_eq!(response, Ok(EndpointReply::Done));
    assert!(app.state.workspaces[0].tabs()[0].zoomed());
    assert_eq!(app.state.workspaces[0].focused_pane_id(), right);
}

#[test]
fn pane_resize_changes_target_ratio_without_changing_focus() {
    let mut app = app_with_workspace();
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let right = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(0, right);
    lay_out_first_tab(&mut app);
    let root_public = app.public_pane_id(0, root).expect("test precondition");

    let response = app.handle_pane_resize(&PaneResizeParams {
        pane_id: Some(root_public.to_string()),
        direction: PaneDirection::Right,
        amount: Some(0.1),
    });

    assert_eq!(response, Ok(EndpointReply::Done));
    let area = shepr_mux::workspace::layout_rect(app.state.tab_layout_area(0, 0));
    let splits = app.state.workspaces[0].tabs()[0].layout().splits(area);
    assert!((splits[0].ratio - 0.6).abs() < f32::EPSILON);
    assert_eq!(app.state.workspaces[0].focused_pane_id(), right);
}

#[test]
fn pane_focus_direction_focuses_neighbor() {
    let mut app = app_with_workspace();
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    let right = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane_in_tab(0, root);
    lay_out_first_tab(&mut app);
    let root_public = app.public_pane_id(0, root).expect("test precondition");

    let response = app.handle_pane_focus_direction(&PaneFocusDirectionParams {
        pane_id: Some(root_public.to_string()),
        direction: PaneDirection::Right,
    });

    assert_eq!(response, Ok(EndpointReply::Done));
    assert_eq!(app.state.workspaces[0].focused_pane_id(), right);
}

#[test]
fn pane_focus_focuses_direct_target_across_tabs_and_workspaces() {
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

    let response = app.handle_pane_focus(&PaneTarget {
        pane_id: target_public.to_string(),
    });

    let Ok(EndpointReply::PaneInfo { pane }) = response else {
        panic!("expected pane info, got {response:?}");
    };
    assert_eq!(pane.pane_id, target_public);
    assert_eq!(app.state.active_index(), Some(1));
    assert_eq!(app.state.workspaces[1].active_tab_index(), target_tab_idx);
    assert_eq!(app.state.workspaces[1].focused_pane_id(), target_pane);
    assert_eq!(app.state.mode, Mode::Terminal);
}

#[test]
fn pane_focus_returns_idle_agent_status() {
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

    let Ok(EndpointReply::PaneInfo { pane }) = response else {
        panic!("expected pane info, got {response:?}");
    };
    assert_eq!(pane.agent_status, shepr_protocol::AgentStatus::Idle);
}

#[test]
fn pane_focus_rejects_invalid_pane_id() {
    let mut app = app_with_workspace();

    let response = app.handle_pane_focus(&PaneTarget {
        pane_id: "pane_missing".into(),
    });

    assert_eq!(
        response.expect_err("an unknown pane is refused").code,
        ApiErrorCode::PaneNotFound
    );
}

#[test]
fn pane_focus_direction_without_a_neighbor_is_a_noop() {
    let mut app = app_with_workspace();
    let root = app.state.workspaces[0].tabs()[0].root_pane();
    app.state.workspaces[0].focus_pane_in_tab(0, root);
    lay_out_first_tab(&mut app);
    let root_public = app.public_pane_id(0, root).expect("test precondition");

    let response = app.handle_pane_focus_direction(&PaneFocusDirectionParams {
        pane_id: Some(root_public.to_string()),
        direction: PaneDirection::Left,
    });

    assert_eq!(response, Ok(EndpointReply::Done));
    assert_eq!(app.state.workspaces[0].focused_pane_id(), root);
}
