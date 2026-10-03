use super::*;
use crate::app::SpawnGeometry;
use crate::test_support::*;
use shepr_config::ServerConfig;
use shepr_mux::workspace::Workspace;
use shepr_protocol::command::{EndpointCommand, EndpointError};
use shepr_protocol::{PublicPaneId, WorkspaceId};
use shepr_termio::host_term::cell_size::HostCellSize;

fn app_with_test_workspace() -> (App, PublicPaneId) {
    let mut app = App::new(&ServerConfig::default(), crate::app::AppPolicy::Test);
    app.state.workspaces = vec![Workspace::test_new("metadata")];
    app.state.ensure_test_terminals();
    let pane_id = app.state.workspaces[0].root_pane();
    let public_pane_id = app.public_pane_id(0, pane_id).expect("test precondition");
    (app, public_pane_id)
}

/// A pane id in a workspace that does not exist.
fn missing_pane() -> PublicPaneId {
    PublicPaneId::new(&WorkspaceId::from_number(9_999).expect("nonzero number"), 1)
}

fn ctx() -> EndpointContext {
    EndpointContext::without_geometry()
}

#[test]
fn pane_input_set_changes_only_the_target_pane() {
    let (mut app, public_pane_id) = app_with_test_workspace();
    let target = app.state.workspaces[0].root_pane();
    let other = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);

    let handled = app
        .handle_pane_input_set(&PaneInputSetParams {
            pane_id: public_pane_id,
            right_click: shepr_protocol::command::PaneRightClickTarget::Pane,
        })
        .expect("input is set");

    assert_eq!(handled.reply, EndpointReply::Done);
    assert_eq!(handled.navigate, None);
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

fn app_with_scrollback_runtime() -> (App, PublicPaneId, PaneId) {
    let (mut app, public_pane_id) = app_with_test_workspace();
    let pane_id = app.state.workspaces[0].root_pane();
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
    let command = EndpointCommand::PaneClear(PaneTarget {
        pane_id: public_pane_id,
    });
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

    let handled = app
        .handle_pane_scroll(&PaneScrollParams {
            pane_id: public_pane_id,
            offset_from_bottom: usize::MAX,
        })
        .expect("the pane scrolls");

    assert_eq!(handled.navigate, None);
    let EndpointReply::PaneInfo { pane } = handled.reply else {
        panic!("expected pane info, got {:?}", handled.reply);
    };
    assert_eq!(
        pane.scroll.expect("scroll metrics").offset_from_bottom,
        max_offset
    );
}

#[tokio::test]
async fn pane_selection_read_uses_endpoint_terminal_text() {
    let (mut app, public_pane_id) = app_with_test_workspace();
    let pane_id = app.state.workspaces[0].root_pane();
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
    let handled = app
        .handle_pane_selection_read(params)
        .expect("the selection reads");

    assert_eq!(
        handled.reply,
        EndpointReply::PaneSelection {
            pane_id: public_pane_id,
            text: "hello".into(),
        }
    );
}

#[tokio::test]
async fn copy_motion_uses_endpoint_terminal_word_semantics() {
    let (mut app, public_pane_id) = app_with_test_workspace();
    let pane_id = app.state.workspaces[0].root_pane();
    app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_scrollback_bytes(20, 5, 1000, b"hello world"),
    );

    let handled = app
        .handle_pane_copy_motion(PaneCopyMotionParams {
            pane_id: public_pane_id.clone(),
            cursor: PaneTextPoint {
                row: shepr_vt::AbsRow(0),
                col: 0,
            },
            motion: PaneCopyMotion::Word(PaneWordMotion::NextStart),
        })
        .expect("the motion resolves");

    assert_eq!(
        handled.reply,
        EndpointReply::PaneCopyMotion {
            pane_id: public_pane_id,
            cursor: PaneTextPoint {
                row: shepr_vt::AbsRow(0),
                col: 6
            },
        }
    );
}

#[tokio::test]
async fn line_motions_land_on_the_ends_of_the_row() {
    let (mut app, public_pane_id) = app_with_test_workspace();
    let pane_id = app.state.workspaces[0].root_pane();
    app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_scrollback_bytes(20, 5, 1000, b"  hello  "),
    );

    for (motion, col) in [(PaneLineMotion::FirstNonBlank, 2), (PaneLineMotion::End, 6)] {
        let handled = app
            .handle_pane_copy_motion(PaneCopyMotionParams {
                pane_id: public_pane_id.clone(),
                cursor: PaneTextPoint {
                    row: shepr_vt::AbsRow(0),
                    col: 0,
                },
                motion: PaneCopyMotion::Line(motion),
            })
            .expect("the motion resolves");
        assert_eq!(
            handled.reply,
            EndpointReply::PaneCopyMotion {
                pane_id: public_pane_id.clone(),
                cursor: PaneTextPoint {
                    row: shepr_vt::AbsRow(0),
                    col
                },
            },
            "{motion:?}"
        );
    }
}

/// Output that evicts history between two requests does not move the line a
/// copy-mode point names: motions and searches read absolute rows.
#[tokio::test]
async fn copy_motion_and_search_keep_their_line_across_eviction() {
    let (mut app, public_pane_id) = app_with_test_workspace();
    let pane_id = app.state.workspaces[0].root_pane();
    let initial: String = (0..1_100).map(|i| format!("{i:06}\r\n")).collect();
    // One byte of scrollback budget buys the minimum history, so every
    // further line evicts one.
    app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_scrollback_bytes(10, 3, 1, initial.as_bytes()),
    );
    let search = |app: &mut App| {
        let handled = app
            .handle_pane_copy_search(PaneCopySearchParams {
                pane_id: public_pane_id.clone(),
                query: "001099".into(),
                direction: PaneCopySearchDirection::Forward,
                cursor: PaneTextPoint {
                    row: shepr_vt::AbsRow(0),
                    col: 0,
                },
                previous: None,
            })
            .expect("the search runs");
        let EndpointReply::PaneCopySearch { matches, .. } = handled.reply else {
            panic!("expected copy search, got {:?}", handled.reply);
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
    let handled = app
        .handle_pane_copy_motion(PaneCopyMotionParams {
            pane_id: public_pane_id.clone(),
            cursor: line,
            motion: PaneCopyMotion::Word(PaneWordMotion::NextEnd),
        })
        .expect("the motion resolves");
    assert_eq!(
        handled.reply,
        EndpointReply::PaneCopyMotion {
            pane_id: public_pane_id.clone(),
            cursor: PaneTextPoint {
                row: line.row,
                col: 5
            },
        }
    );

    // A line motion on a row history has evicted is refused.
    let evicted = app.handle_pane_copy_motion(PaneCopyMotionParams {
        pane_id: public_pane_id,
        cursor: PaneTextPoint {
            row: shepr_vt::AbsRow(0),
            col: 0,
        },
        motion: PaneCopyMotion::Line(PaneLineMotion::End),
    });
    let error = evicted.expect_err("an evicted row is refused");
    assert_eq!(
        error.error,
        EndpointError::Rejected("terminal row is unavailable".into())
    );
}

#[tokio::test]
async fn paragraph_motion_preserves_the_copy_cursor_column() {
    let (mut app, public_pane_id) = app_with_test_workspace();
    let pane_id = app.state.workspaces[0].root_pane();
    app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_scrollback_bytes(20, 5, 1000, b"one\r\n\r\nthree"),
    );
    let handled = app
        .handle_pane_copy_motion(PaneCopyMotionParams {
            pane_id: public_pane_id.clone(),
            cursor: PaneTextPoint {
                row: shepr_vt::AbsRow(0),
                col: 2,
            },
            motion: PaneCopyMotion::Paragraph(PaneParagraphMotion::Next),
        })
        .expect("the motion resolves");
    assert_eq!(
        handled.reply,
        EndpointReply::PaneCopyMotion {
            pane_id: public_pane_id,
            cursor: PaneTextPoint {
                row: shepr_vt::AbsRow(1),
                col: 2
            },
        }
    );
}

#[tokio::test]
async fn copy_search_uses_endpoint_terminal_matches_and_wraps() {
    let (mut app, public_pane_id) = app_with_test_workspace();
    let pane_id = app.state.workspaces[0].root_pane();
    app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_scrollback_bytes(20, 5, 1000, b"alpha beta alpha"),
    );

    let handled = app
        .handle_pane_copy_search(PaneCopySearchParams {
            pane_id: public_pane_id.clone(),
            query: "alpha".into(),
            direction: PaneCopySearchDirection::Forward,
            cursor: PaneTextPoint {
                row: shepr_vt::AbsRow(0),
                col: 0,
            },
            previous: None,
        })
        .expect("the search runs");

    let EndpointReply::PaneCopySearch {
        pane_id,
        matches,
        current,
        total,
        current_global,
    } = handled.reply
    else {
        panic!("expected copy search");
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
    let pane_id = app.state.workspaces[0].root_pane();
    let text = "a ".repeat(1500);
    app.insert_test_runtime(
        pane_id,
        shepr_mux::pane::PaneRuntime::test_with_scrollback_bytes(200, 20, 4000, text.as_bytes()),
    );
    let handled = app
        .handle_pane_copy_search(PaneCopySearchParams {
            pane_id: public_pane_id,
            query: "a".into(),
            direction: PaneCopySearchDirection::Forward,
            cursor: PaneTextPoint {
                row: shepr_vt::AbsRow(0),
                col: 0,
            },
            previous: None,
        })
        .expect("the search runs");
    let EndpointReply::PaneCopySearch { matches, total, .. } = handled.reply else {
        panic!("expected copy search");
    };
    assert_eq!(total, 1500);
    assert_eq!(matches.len(), 1024);
}

#[test]
fn pane_rename_returns_the_renamed_pane() {
    let (mut app, public_pane_id) = app_with_test_workspace();

    let handled = app
        .handle_pane_rename(PaneRenameParams {
            pane_id: public_pane_id.clone(),
            label: Some("build".into()),
        })
        .expect("the pane is renamed");

    let EndpointReply::PaneInfo { pane } = handled.reply else {
        panic!("expected pane info");
    };
    assert_eq!(pane.pane_id, public_pane_id);
    assert_eq!(handled.navigate, None);
    let pane_id = app.state.workspaces[0].root_pane();
    let terminal_id = app.state.workspaces[0]
        .pane_state(pane_id)
        .expect("test precondition")
        .attached_terminal_id
        .clone();
    assert_eq!(
        app.state.terminals[&terminal_id].manual_label(),
        Some("build")
    );
}

#[test]
fn pane_rename_sets_and_clears_the_manual_label() {
    let (mut app, public_pane_id) = app_with_test_workspace();
    let pane = app.state.workspaces[0].root_pane();
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
        app.state.terminals[&terminal_id].manual_label(),
        Some("reviewer")
    );

    let response = app.handle_pane_rename(PaneRenameParams {
        pane_id: public_pane_id,
        label: None,
    });
    assert!(matches!(
        response,
        Ok(Handled {
            reply: EndpointReply::PaneInfo { .. },
            ..
        })
    ));
    assert!(app.state.terminals[&terminal_id].manual_label().is_none());
}

fn app_with_workspace() -> App {
    let mut app = App::new(&ServerConfig::default(), crate::app::AppPolicy::Test);
    app.state.workspaces = vec![Workspace::test_new("issue")];
    app.state.ensure_test_terminals();
    app
}

#[test]
fn pane_close_of_last_pane_closes_workspace() {
    let mut app = app_with_workspace();
    let pane_id = app.state.workspaces[0].root_pane();
    let public_pane_id = app.public_pane_id(0, pane_id).expect("test precondition");

    let handled = app
        .handle_pane_close(&PaneTarget {
            pane_id: public_pane_id,
        })
        .expect("the pane closes");

    assert_eq!(handled.reply, EndpointReply::Done);
    assert_eq!(handled.navigate, None);
    assert!(app.state.workspaces.is_empty());
}

#[test]
fn pane_close_keeps_the_workspace_when_other_panes_remain() {
    let mut app = app_with_workspace();
    let root = app.state.workspaces[0].root_pane();
    let survivor = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
    app.state.ensure_test_terminals();
    let public_pane_id = app.public_pane_id(0, root).expect("test precondition");

    app.handle_pane_close(&PaneTarget {
        pane_id: public_pane_id,
    })
    .expect("the pane closes");

    assert_eq!(app.state.workspaces.len(), 1);
    assert_eq!(app.state.workspaces[0].pane_count(), 1);
    assert!(app.state.workspaces[0].contains_pane(survivor));
}

/// Lays the first workspace out in a 100x20 area, as the server does when it
/// applies PTY geometry.
fn lay_out_first_workspace(app: &mut App) {
    let id = app.state.workspaces[0].id.clone();
    app.state.record_workspace_geometry(
        &id,
        SpawnGeometry {
            area: ratatui::layout::Rect::new(0, 0, 100, 20),
            cell_size: HostCellSize::default(),
        },
    );
}

/// The panes of the first workspace in layout order.
fn workspace_pane_order(app: &App) -> Vec<PaneId> {
    app.state.workspaces[0].layout().pane_ids()
}

/// A workspace of two panes, side by side and laid out, with `root` focused.
fn app_with_two_panes() -> (App, PaneId, PaneId) {
    let mut app = app_with_workspace();
    let root = app.state.workspaces[0].root_pane();
    let right = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
    app.state.ensure_test_terminals();
    app.state.workspaces[0].focus_pane(root);
    lay_out_first_workspace(&mut app);
    (app, root, right)
}

#[test]
fn pane_swap_explicit_panes_swap_and_keep_focus_on_the_source() {
    let mut app = app_with_workspace();
    let source = app.state.workspaces[0].root_pane();
    let target = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane(source);
    lay_out_first_workspace(&mut app);
    let source_public = app.public_pane_id(0, source).expect("test precondition");
    let target_public = app.public_pane_id(0, target).expect("test precondition");
    assert_eq!(workspace_pane_order(&app), vec![source, target]);

    let handled = app
        .handle_pane_swap(&PaneSwapParams::Panes {
            source: source_public,
            target: target_public,
        })
        .expect("the swap succeeds");

    assert_eq!(handled.reply, EndpointReply::Done);
    assert_eq!(handled.navigate, app.public_workspace_id(0));
    assert_eq!(workspace_pane_order(&app), vec![target, source]);
    assert_eq!(app.state.workspaces[0].focused_pane_id(), source);
}

#[test]
fn pane_swap_by_direction_swaps_with_the_neighbour_and_navigates() {
    let (mut app, root, right) = app_with_two_panes();
    let root_public = app.public_pane_id(0, root).expect("test precondition");

    let handled = app
        .handle_pane_swap(&PaneSwapParams::Direction {
            pane_id: root_public,
            direction: PaneDirection::Right,
        })
        .expect("the swap succeeds");

    assert_eq!(handled.navigate, app.public_workspace_id(0));
    assert_eq!(workspace_pane_order(&app), vec![right, root]);
}

#[test]
fn pane_swap_without_a_neighbor_is_a_noop_that_moves_nobody() {
    let mut app = app_with_workspace();
    let source = app.state.workspaces[0].root_pane();
    lay_out_first_workspace(&mut app);
    let source_public = app.public_pane_id(0, source).expect("test precondition");

    let handled = app
        .handle_pane_swap(&PaneSwapParams::Direction {
            pane_id: source_public,
            direction: PaneDirection::Left,
        })
        .expect("nothing to swap is a success");

    assert_eq!(handled.reply, EndpointReply::Done);
    assert_eq!(handled.navigate, None, "the layout did not change");
    assert_eq!(workspace_pane_order(&app), vec![source]);
}

#[test]
fn pane_swap_with_an_unknown_pane_is_refused_by_direction_and_a_noop_by_id() {
    let (mut app, root, right) = app_with_two_panes();
    let root_public = app.public_pane_id(0, root).expect("test precondition");

    // A direction from a pane that is gone has no source to swap.
    let refused = app.handle_pane_swap(&PaneSwapParams::Direction {
        pane_id: missing_pane(),
        direction: PaneDirection::Right,
    });
    assert!(matches!(
        refused,
        Err(error) if matches!(error.error, EndpointError::Rejected(_))
    ));

    // Stale explicit ids are a successful no-op, whichever one is stale.
    for (source, target) in [
        (missing_pane(), root_public.clone()),
        (root_public.clone(), missing_pane()),
    ] {
        let handled = app
            .handle_pane_swap(&PaneSwapParams::Panes { source, target })
            .expect("a stale swap is a no-op");
        assert_eq!(handled.reply, EndpointReply::Done);
        assert_eq!(handled.navigate, None);
        assert_eq!(workspace_pane_order(&app), vec![root, right]);
    }

    // The same pane twice swaps nothing either.
    let handled = app
        .handle_pane_swap(&PaneSwapParams::Panes {
            source: root_public.clone(),
            target: root_public,
        })
        .expect("an identical swap is a no-op");
    assert_eq!(handled.navigate, None);
    assert_eq!(workspace_pane_order(&app), vec![root, right]);
}

#[test]
fn pane_swap_across_workspaces_is_a_noop() {
    let mut app = app_with_workspace();
    app.state.workspaces.push(Workspace::test_new("other"));
    let source = app.state.workspaces[0].root_pane();
    let target = app.state.workspaces[1].root_pane();
    let source_public = app.public_pane_id(0, source).expect("test precondition");
    let target_public = app.public_pane_id(1, target).expect("test precondition");

    let handled = app
        .handle_pane_swap(&PaneSwapParams::Panes {
            source: source_public,
            target: target_public,
        })
        .expect("a cross-workspace swap is a no-op");

    assert_eq!(handled.reply, EndpointReply::Done);
    assert_eq!(handled.navigate, None);
    assert_eq!(app.state.workspaces[0].root_pane(), source);
    assert_eq!(app.state.workspaces[1].root_pane(), target);
}

#[test]
fn pane_zoom_toggles_zoom_and_navigates() {
    let mut app = app_with_workspace();
    let root = app.state.workspaces[0].root_pane();
    let _right = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane(root);
    let root_public = app.public_pane_id(0, root).expect("test precondition");
    let params = PaneZoomParams {
        pane_id: root_public,
    };

    let handled = app.handle_pane_zoom(&params).expect("the pane zooms");

    assert_eq!(handled.reply, EndpointReply::Done);
    assert_eq!(handled.navigate, app.public_workspace_id(0));
    assert!(app.state.workspaces[0].zoomed());
    assert_eq!(app.state.workspaces[0].focused_pane_id(), root);

    app.handle_pane_zoom(&params).expect("the pane unzooms");
    assert!(!app.state.workspaces[0].zoomed());
}

#[test]
fn pane_zoom_of_a_single_pane_changes_nothing_but_still_navigates() {
    let mut app = app_with_workspace();
    let root = app.state.workspaces[0].root_pane();
    let root_public = app.public_pane_id(0, root).expect("test precondition");

    let handled = app
        .handle_pane_zoom(&PaneZoomParams {
            pane_id: root_public,
        })
        .expect("a lone pane is a valid target");

    assert_eq!(handled.reply, EndpointReply::Done);
    assert_eq!(handled.navigate, app.public_workspace_id(0));
    assert!(!app.state.workspaces[0].zoomed());
}

#[test]
fn pane_zoom_on_another_pane_of_a_zoomed_workspace_focuses_it_and_toggles() {
    let mut app = app_with_workspace();
    let root = app.state.workspaces[0].root_pane();
    let right = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane(root);
    app.state.workspaces[0].set_zoomed(true);
    let right_public = app.public_pane_id(0, right).expect("test precondition");

    let handled = app
        .handle_pane_zoom(&PaneZoomParams {
            pane_id: right_public,
        })
        .expect("the toggle succeeds");

    assert_eq!(handled.navigate, app.public_workspace_id(0));
    assert_eq!(app.state.workspaces[0].focused_pane_id(), right);
    assert!(!app.state.workspaces[0].zoomed());
}

#[test]
fn pane_resize_changes_target_ratio_without_changing_focus_or_navigating() {
    let mut app = app_with_workspace();
    let root = app.state.workspaces[0].root_pane();
    let right = app.state.workspaces[0].test_split(shepr_core::layout::Direction::Horizontal);
    app.state.workspaces[0].focus_pane(right);
    lay_out_first_workspace(&mut app);
    let root_public = app.public_pane_id(0, root).expect("test precondition");

    let handled = app
        .handle_pane_resize(&PaneResizeParams {
            pane_id: root_public,
            direction: PaneDirection::Right,
        })
        .expect("the resize succeeds");

    assert_eq!(handled.reply, EndpointReply::Done);
    assert_eq!(handled.navigate, None);
    let area = shepr_mux::workspace::layout_rect(app.state.workspace_layout_area(0));
    let splits = app.state.workspaces[0].layout().splits(area);
    assert!((splits[0].ratio.get() - 0.55).abs() < 1e-6);
    assert_eq!(app.state.workspaces[0].focused_pane_id(), right);
}

#[test]
fn pane_focus_direction_focuses_the_neighbor_and_navigates() {
    let (mut app, root, right) = app_with_two_panes();
    let root_public = app.public_pane_id(0, root).expect("test precondition");

    let handled = app
        .handle_pane_focus_direction(&PaneFocusDirectionParams {
            pane_id: root_public,
            direction: PaneDirection::Right,
        })
        .expect("the neighbour is focused");

    assert_eq!(handled.reply, EndpointReply::Done);
    assert_eq!(handled.navigate, app.public_workspace_id(0));
    assert_eq!(app.state.workspaces[0].focused_pane_id(), right);
}

#[test]
fn pane_focus_direction_navigates_even_when_the_neighbor_already_has_focus() {
    let (mut app, root, right) = app_with_two_panes();
    app.state.workspaces[0].focus_pane(right);
    let root_public = app.public_pane_id(0, root).expect("test precondition");

    let handled = app
        .handle_pane_focus_direction(&PaneFocusDirectionParams {
            pane_id: root_public,
            direction: PaneDirection::Right,
        })
        .expect("the neighbour is found");

    assert_eq!(handled.navigate, app.public_workspace_id(0));
    assert_eq!(app.state.workspaces[0].focused_pane_id(), right);
}

#[test]
fn pane_focus_direction_without_a_neighbor_moves_nobody() {
    let mut app = app_with_workspace();
    let root = app.state.workspaces[0].root_pane();
    lay_out_first_workspace(&mut app);
    let root_public = app.public_pane_id(0, root).expect("test precondition");

    let handled = app
        .handle_pane_focus_direction(&PaneFocusDirectionParams {
            pane_id: root_public,
            direction: PaneDirection::Left,
        })
        .expect("an edge is a success");

    assert_eq!(handled.reply, EndpointReply::Done);
    assert_eq!(handled.navigate, None, "an edge navigates nobody");
    assert_eq!(app.state.workspaces[0].focused_pane_id(), root);
}

#[test]
fn pane_focus_focuses_the_target_and_navigates_across_workspaces() {
    let mut app = app_with_workspace();
    app.state.workspaces.push(Workspace::test_new("other"));
    let target_pane = app.state.workspaces[1].root_pane();
    app.state.ensure_test_terminals();
    let target_public = app
        .public_pane_id(1, target_pane)
        .expect("test precondition");
    app.state.set_bookmark_index(Some(0));

    let handled = app
        .handle_pane_focus(&PaneTarget {
            pane_id: target_public.clone(),
        })
        .expect("the pane is focused");

    let EndpointReply::PaneInfo { pane } = handled.reply else {
        panic!("expected pane info");
    };
    assert_eq!(pane.pane_id, target_public);
    assert_eq!(handled.navigate, app.public_workspace_id(1));
    assert_eq!(app.state.workspaces[1].focused_pane_id(), target_pane);
    // The app applies no navigation: the bookmark moves only from an active
    // client's effect, applied by the server loop.
    assert_eq!(app.state.bookmark_index(), Some(0));
}

#[test]
fn pane_focus_on_the_focused_pane_still_navigates() {
    let mut app = app_with_workspace();
    let pane_id = app.state.workspaces[0].root_pane();
    let public_pane_id = app.public_pane_id(0, pane_id).expect("test precondition");
    app.state.session_dirty = false;

    let handled = app
        .handle_pane_focus(&PaneTarget {
            pane_id: public_pane_id.clone(),
        })
        .expect("the pane is focused");

    assert_eq!(handled.navigate, app.public_workspace_id(0));
    assert!(!app.state.session_dirty, "nothing was mutated");
    let EndpointReply::PaneInfo { pane } = handled.reply else {
        panic!("expected pane info");
    };
    assert_eq!(pane.pane_id, public_pane_id);
}

#[test]
fn a_rejected_focus_command_moves_nobody() {
    let (mut app, root, _right) = app_with_two_panes();
    let gone = WorkspaceId::from_number(9_999).expect("nonzero number");
    let commands = [
        EndpointCommand::WorkspaceFocus(shepr_protocol::command::WorkspaceTarget {
            workspace_id: gone,
        }),
        EndpointCommand::PaneFocus(PaneTarget {
            pane_id: missing_pane(),
        }),
        EndpointCommand::PaneFocusDirection(PaneFocusDirectionParams {
            pane_id: missing_pane(),
            direction: PaneDirection::Right,
        }),
        EndpointCommand::PaneZoom(PaneZoomParams {
            pane_id: missing_pane(),
        }),
        EndpointCommand::PaneSwap(PaneSwapParams::Direction {
            pane_id: missing_pane(),
            direction: PaneDirection::Right,
        }),
        EndpointCommand::PaneSplit(PaneSplitParams {
            pane_id: missing_pane(),
            direction: shepr_protocol::command::SplitDirection::Right,
        }),
    ];

    for command in commands {
        let name = command.name();
        let outcome = app.handle_endpoint_command_in(command, &ctx());
        assert!(
            matches!(outcome.result, Err(EndpointError::Rejected(_))),
            "{name}"
        );
        assert_eq!(outcome.navigate, None, "{name}");
    }
    assert_eq!(app.state.workspaces[0].focused_pane_id(), root);
}

#[test]
fn commands_that_only_change_state_in_place_navigate_nobody() {
    let (mut app, root, right) = app_with_two_panes();
    let root_public = app.public_pane_id(0, root).expect("test precondition");
    let right_public = app.public_pane_id(0, right).expect("test precondition");
    let workspace_id = app.public_workspace_id(0).expect("test precondition");
    let commands = [
        EndpointCommand::PaneInputSet(PaneInputSetParams {
            pane_id: root_public.clone(),
            right_click: shepr_protocol::command::PaneRightClickTarget::Pane,
        }),
        EndpointCommand::PaneRename(PaneRenameParams {
            pane_id: root_public.clone(),
            label: Some("x".into()),
        }),
        EndpointCommand::PaneResize(PaneResizeParams {
            pane_id: root_public.clone(),
            direction: PaneDirection::Right,
        }),
        EndpointCommand::WorkspaceRename(shepr_protocol::command::WorkspaceRenameParams {
            workspace_id: workspace_id.clone(),
            label: Some("renamed".into()),
        }),
        EndpointCommand::LayoutSetSplitRatio(shepr_protocol::command::LayoutSetSplitRatioParams {
            workspace_id: workspace_id.clone(),
            first_panes: vec![root_public],
            second_panes: vec![right_public],
            ratio: shepr_core::layout::SplitRatio::new(0.4).expect("test split ratio is valid"),
        }),
        EndpointCommand::WorkspaceMove(shepr_protocol::command::WorkspaceMoveParams {
            workspace_id,
            before_workspace_id: None,
        }),
    ];

    for command in commands {
        let name = command.name();
        let outcome = app.handle_endpoint_command_in(command, &ctx());
        assert!(outcome.result.is_ok(), "{name}: {:?}", outcome.result);
        assert_eq!(outcome.navigate, None, "{name}");
    }
}

#[test]
fn pane_focus_rejects_a_pane_that_is_gone() {
    let mut app = app_with_workspace();

    let response = app.handle_pane_focus(&PaneTarget {
        pane_id: missing_pane(),
    });

    let error = response.expect_err("a missing pane is refused");
    assert_eq!(
        error.error,
        EndpointError::Rejected(format!("pane {} not found", missing_pane()))
    );
}
