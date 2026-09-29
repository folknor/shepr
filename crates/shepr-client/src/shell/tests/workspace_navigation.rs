use super::*;

fn workspaces(count: usize) -> ClientShellSnapshot {
    let mut projected = snapshot();
    projected.workspaces = (1..=count)
        .map(|number| {
            let mut workspace = projected.workspaces[0].clone();
            workspace.workspace_id = test_workspace_id(&format!("w{number}"));
            workspace.number = number;
            workspace.focused = number == 1;
            workspace
        })
        .collect();
    projected
}

fn navigation_state(mut projected: ClientShellSnapshot) -> (ClientShellState, ClientEndpointId) {
    let (mut state, remote) = state_with_remote();
    state.set_snapshot(Box::new(projected.clone()));
    projected.boot_id = crate::tests::test_boot_id("remote-boot");
    state.set_endpoint_snapshot(&remote, Box::new(projected));
    (state, remote)
}

fn preview_key(state: &mut ClientShellState, bytes: &[u8]) {
    let outcome = state.handle_input_bytes(bytes);
    assert!(outcome.actions.is_empty(), "{bytes:?}");
    assert!(outcome.requests.is_empty(), "{bytes:?}");
    assert!(outcome.repaint, "{bytes:?}");
}

fn enter_navigation(state: &mut ClientShellState) {
    preview_key(state, &[0x02]);
    preview_key(state, b"w");
    assert_eq!(state.mode, ClientShellMode::Navigate);
}

fn assert_selected(state: &ClientShellState, endpoint: &ClientEndpointId, workspace: &str) {
    assert_eq!(
        state.navigate_workspace_id,
        state.navigation_target(endpoint, &shepr_test_fixtures::id(workspace))
    );
}

fn workspace_rect(state: &ClientShellState, endpoint: &ClientEndpointId, workspace: &str) -> Rect {
    state
        .hits
        .workspaces
        .iter()
        .find(|hit| &hit.endpoint_id == endpoint && hit.workspace_id == workspace)
        .map(|hit| hit.rect)
        .expect("visible workspace")
}

#[test]
fn local_navigation_highlight_stays_visible_with_terminal_theme() {
    use ratatui::style::Color;

    for compact in [false, true] {
        for selection_bg in [Color::Reset, Color::Rgb(70, 63, 93)] {
            let mut config = ClientShellConfig::from_config(&Config::default());
            config.palette = Palette::terminal();
            config.palette.selection_bg = selection_bg;
            let expected_bg = if selection_bg == Color::Reset {
                config.palette.active_row_bg
            } else {
                selection_bg
            };
            let mut state = ClientShellState::new(config);
            state.set_snapshot(Box::new(workspaces(3)));
            state.set_pane_surface(surface());
            state.sidebar_collapsed = compact;
            state.compose(100, 28).expect("test precondition");
            enter_navigation(&mut state);

            for workspace_id in ["w1", "w2"] {
                assert_selected(&state, &ClientEndpointId::Local, workspace_id);
                let buffer = state
                    .compose(100, 28)
                    .expect("test precondition")
                    .to_ratatui_buffer()
                    .expect("test precondition");
                let selected = workspace_rect(&state, &ClientEndpointId::Local, workspace_id);
                for y in selected.y..selected.bottom() {
                    for x in selected.x..selected.right() {
                        assert_eq!(
                            buffer[(x, y)].bg,
                            expected_bg,
                            "compact={compact}, {workspace_id}, ({x}, {y})"
                        );
                    }
                }
                let untouched = workspace_rect(&state, &ClientEndpointId::Local, "w3");
                assert_ne!(buffer[(untouched.x, untouched.y)].bg, expected_bg);
                if workspace_id != "w1" {
                    let focused = workspace_rect(&state, &ClientEndpointId::Local, "w1");
                    assert_eq!(
                        buffer[(focused.x, focused.y)].bg,
                        if selection_bg == Color::Reset {
                            state.config.palette.sidebar_bg
                        } else {
                            state.config.palette.active_row_bg
                        }
                    );
                    assert_ne!(buffer[(focused.x, focused.y)].bg, expected_bg);
                }
                preview_key(&mut state, b"\x1b[B");
            }
            assert_eq!(
                state
                    .snapshot
                    .as_ref()
                    .expect("test precondition")
                    .focused_workspace_id
                    .as_deref(),
                Some("w1")
            );
            preview_key(&mut state, b"\x1b");
            let buffer = state
                .compose(100, 28)
                .expect("test precondition")
                .to_ratatui_buffer()
                .expect("test precondition");
            let focused = workspace_rect(&state, &ClientEndpointId::Local, "w1");
            assert_eq!(
                buffer[(focused.x, focused.y)].bg,
                state.config.palette.active_row_bg
            );
            let cancelled = workspace_rect(&state, &ClientEndpointId::Local, "w3");
            assert_eq!(
                buffer[(cancelled.x, cancelled.y)].bg,
                state.config.palette.sidebar_bg
            );
        }
    }
}

#[test]
fn navigation_highlights_only_the_preview_and_activates_on_enter() {
    for (compact, cols) in [(true, 100), (false, 100), (false, 44)] {
        for terminal_theme in [false, true] {
            let (mut state, remote) = navigation_state(workspaces(2));
            state.sidebar_collapsed = compact;
            if terminal_theme {
                state.config.palette = Palette::terminal();
            }
            state.compose(cols, 28).expect("test precondition");
            enter_navigation(&mut state);
            for (endpoint, collision, steps) in [
                (&ClientEndpointId::Local, &remote, 1),
                (&remote, &ClientEndpointId::Local, 2),
            ] {
                for _ in 0..steps {
                    preview_key(&mut state, b"\x1b[B");
                }
                assert_selected(&state, endpoint, "w2");
                let buffer = state
                    .compose(cols, 28)
                    .expect("test precondition")
                    .to_ratatui_buffer()
                    .expect("test precondition");
                let selected = workspace_rect(&state, endpoint, "w2");
                let other = workspace_rect(&state, collision, "w2");
                let focused = workspace_rect(&state, &ClientEndpointId::Local, "w1");
                let palette = &state.config.palette;
                let color = if cols == 44 {
                    palette.surface0
                } else {
                    palette.selection_bg
                };
                let color = if color == ratatui::style::Color::Reset {
                    palette.active_row_bg
                } else {
                    color
                };
                assert_eq!(buffer[(selected.x + 2, selected.y)].bg, color);
                assert_ne!(buffer[(other.x + 2, other.y)].bg, color);
                assert_eq!(
                    buffer[(focused.x + 2, focused.y)].bg,
                    if terminal_theme {
                        if cols == 44 {
                            palette.panel_bg
                        } else {
                            palette.sidebar_bg
                        }
                    } else if cols == 44 {
                        palette.surface_dim
                    } else {
                        palette.active_row_bg
                    }
                );
            }
            assert_eq!(
                state.snapshot.as_ref().expect("test precondition").boot_id,
                crate::tests::test_boot_id("boot-1")
            );
            assert_eq!(
                state
                    .snapshot
                    .as_ref()
                    .expect("test precondition")
                    .focused_workspace_id
                    .as_deref(),
                Some("w1")
            );
            assert_eq!(
                state
                    .pane_surface
                    .as_ref()
                    .expect("test precondition")
                    .boot_id,
                crate::tests::test_boot_id("boot-1")
            );
            let enter = state.handle_input_bytes(b"\r");
            assert!(enter.requests.is_empty());
            assert!(
                matches!(enter.actions.as_slice(), [ClientShellAction::ActivateEndpoint {
                endpoint_id, target: Some(ClientEndpointFocusTarget::Workspace(id)),
            }] if endpoint_id == &remote && id == "w2")
            );
            assert_eq!(state.active_endpoint_id, ClientEndpointId::Local);
            assert_eq!(state.mode, ClientShellMode::Terminal);
            assert!(state.navigate_workspace_id.is_none());
        }
    }
}

#[test]
fn foreign_preview_blocks_keyboard_actions_but_keeps_active_action_context() {
    let (mut state, remote) = state_with_remote();
    state.compose(100, 28).expect("test precondition");
    enter_navigation(&mut state);
    preview_key(&mut state, b"\x1b[B");
    for confirm in [false, true] {
        state.config.confirm_close = confirm;
        for key in [
            b"W".as_slice(),
            b"D",
            b"\x1b[D",
            b"\x1b[C",
            b"\t",
            b"1",
            b"c",
            b"N",
        ] {
            preview_key(&mut state, key);
            assert!(state.overlay.is_none());
            assert_eq!(state.mode, ClientShellMode::Navigate);
        }
    }
    assert_selected(&state, &remote, "w1");
    let mut remote_snapshot = workspaces(2);
    remote_snapshot.boot_id = crate::tests::test_boot_id("remote-boot");
    state.set_endpoint_snapshot(&remote, Box::new(remote_snapshot));
    preview_key(&mut state, b"\x1b[B");
    assert_selected(&state, &remote, "w2");
    assert_eq!(state.workspace_action_id().as_deref(), Some("w1"));
    state.config.prompt_new_workspace_name = false;
    let mut create = ClientShellInput::default();
    state.record_binding(
        &shepr_termio::input::KeybindMatch::Action(
            shepr_termio::input::KeybindAction::NewWorkspace,
        ),
        &mut create,
    );
    assert!(
        matches!(create.actions.as_slice(), [ClientShellAction::Endpoint { endpoint_id: ClientEndpointId::Local, request, .. }]
        if matches!(&request.method, shepr_api::schema::Method::WorkspaceCreate(params) if params.source_workspace_id.as_deref() == Some("w1")))
    );
    preview_key(&mut state, b"\x1b");
    assert!(state.navigate_workspace_id.is_none());
    assert_eq!(state.active_endpoint_id, ClientEndpointId::Local);
    assert!(state.activate_endpoint_projection(&remote));
    enter_navigation(&mut state);
    preview_key(&mut state, b"W");
    assert!(matches!(state.overlay, Some(ClientShellOverlay::Rename(_))));
}

#[test]
fn blocked_preview_notice_names_the_configured_open_key() {
    let (mut state, _) = state_with_remote();
    state.compose(100, 28).expect("test precondition");
    enter_navigation(&mut state);
    preview_key(&mut state, b"\x1b[B");
    preview_key(&mut state, b"W");
    // The hint is the configured key's label, lowercase like every other
    // key label, so a rebound navigate_open_workspace is named correctly.
    let body = &state
        .visible_endpoint_notice
        .as_ref()
        .expect("blocked preview notice")
        .body;
    assert!(body.contains("press enter before"), "{body}");
}

#[test]
fn navigate_back_matches_its_configured_modifiers_exactly() {
    let (mut state, _) = state_with_remote();
    state.compose(100, 28).expect("test precondition");
    enter_navigation(&mut state);
    // Alt+Esc (kitty encoding) is not the configured "esc": navigate_back
    // matches exactly like every other binding, so navigate mode stays open.
    let alt_esc = state.handle_input_bytes(b"\x1b[27;3u");
    assert!(alt_esc.actions.is_empty() && alt_esc.requests.is_empty());
    assert_eq!(state.mode, ClientShellMode::Navigate);
    preview_key(&mut state, b"\x1b[27u");
    assert_eq!(state.mode, ClientShellMode::Terminal);
}

#[test]
fn empty_workspace_navigation_enter_exits_without_focusing() {
    let (mut state, _) = state_with_remote();
    let mut empty = workspaces(0);
    empty.tabs.clear();
    empty.panes.clear();
    empty.focused_workspace_id = None;
    empty.focused_tab_id = None;
    empty.focused_pane_id = None;
    state.set_snapshot(Box::new(empty));
    enter_navigation(&mut state);
    assert!(state.navigate_workspace_id.is_none());
    let enter = state.handle_input_bytes(b"\r");
    assert!(enter.actions.is_empty() && enter.requests.is_empty() && enter.repaint);
    assert_eq!(state.mode, ClientShellMode::Terminal);
}

#[test]
fn foreign_workspace_preview_blocks_paste_into_hidden_copy_search() {
    let (mut state, _) = state_with_remote();
    let mut pane_surface = surface();
    pane_surface.panes[0].scroll = Some(shepr_protocol::PaneSurfaceScrollMetrics {
        offset_from_bottom: 0,
        max_offset_from_bottom: 20,
        viewport_rows: 2,
        history_origin: shepr_vt::AbsRow(0),
    });
    state.set_pane_surface(pane_surface);
    state.compose(100, 28).expect("test precondition");
    assert!(state.enter_copy_mode(&mut ClientShellInput::default()));
    enter_navigation(&mut state);
    // Seed the hidden prompt after navigation: text editing consumes the prefix key.
    state
        .copy_mode
        .as_mut()
        .expect("test precondition")
        .search_prompt = Some(ClientCopySearchPrompt {
        direction: shepr_api::schema::PaneCopySearchDirection::Forward,
        query: "original".into(),
    });
    preview_key(&mut state, b"\x1b[B");
    assert!(state.workspace_preview_action_blocked());
    assert!(!state.modal_paste_target_active());
    let key = shepr_termio::input::TerminalKey::new(KeyCode::Char('v'), KeyModifiers::CONTROL);
    assert!(!state.handle_modal_paste_shortcut_with(
        &key,
        &mut ClientShellInput::default(),
        || { panic!("hidden search must not read the clipboard") }
    ));
    let paste = state.handle_raw_events(vec![RawInputEvent::Paste("unexpected".into())]);
    assert!(paste.actions.is_empty() && paste.requests.is_empty());
    assert_eq!(
        state
            .copy_mode
            .expect("test precondition")
            .search_prompt
            .expect("test precondition")
            .query,
        "original".into()
    );
}

#[test]
fn mouse_clicks_cancel_remote_workspace_navigation() {
    for pane in [false, true] {
        let (mut state, _) = state_with_remote();
        state.compose(100, 28).expect("test precondition");
        enter_navigation(&mut state);
        preview_key(&mut state, b"\x1b[B");
        state.compose(100, 28).expect("test precondition");
        let rect = if pane {
            state.hits.panes[0].inner_rect
        } else {
            workspace_rect(&state, &ClientEndpointId::Local, "w1")
        };
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
                kind,
                column: rect.x + 2,
                row: rect.y,
                modifiers: KeyModifiers::empty(),
            })]);
        }
        assert_eq!(state.mode, ClientShellMode::Terminal);
        assert!(state.navigate_workspace_id.is_none());
        enter_navigation(&mut state);
        assert_selected(&state, &ClientEndpointId::Local, "w1");
    }
}

#[test]
fn foreign_preview_survives_local_updates_and_rejects_stale_enter() {
    for invalidation in ["offline", "deleted", "boot", "generation"] {
        let (mut state, remote_id) = state_with_remote();
        let mut remote = workspaces(2);
        remote.boot_id = crate::tests::test_boot_id("remote-boot");
        state.set_endpoint_snapshot_for_generation(&remote_id, 7, Box::new(remote.clone()));
        state.compose(100, 28).expect("test precondition");
        enter_navigation(&mut state);
        for _ in 0..2 {
            preview_key(&mut state, b"\x1b[B");
        }
        assert_selected(&state, &remote_id, "w2");
        let selected = state.navigate_workspace_id.clone();
        remote.revision = remote.revision.checked_next().expect("test precondition");
        state.set_endpoint_snapshot_for_generation(&remote_id, 7, Box::new(remote.clone()));
        assert_eq!(state.navigate_workspace_id, selected);
        assert!(state.navigation_target_valid(selected.as_ref().expect("test precondition")));
        let mut local = snapshot();
        local.revision = local.revision.checked_next().expect("test precondition");
        state.set_snapshot(Box::new(local));
        assert_eq!(state.navigate_workspace_id, selected);
        match invalidation {
            "offline" => state.set_endpoint_status(&remote_id, ClientEndpointStatus::Reconnecting),
            "deleted" => {
                remote.revision = remote.revision.checked_next().expect("test precondition");
                remote.workspaces.pop();
                state.set_endpoint_snapshot_for_generation(&remote_id, 7, Box::new(remote));
            }
            "boot" => {
                remote.boot_id = crate::tests::test_boot_id("restarted-remote");
                state.set_endpoint_snapshot_for_generation(&remote_id, 7, Box::new(remote));
            }
            "generation" => {
                state.cache_endpoint_snapshot_for_generation(&remote_id, 8, Box::new(remote));
            }
            _ => unreachable!(),
        }
        preview_key(&mut state, b"\r");
        assert_eq!(state.active_endpoint_id, ClientEndpointId::Local);
        assert_eq!(state.mode, ClientShellMode::Navigate);
        assert!(state.visible_endpoint_notice.is_some());
        assert!(
            !state.navigation_target_valid(
                state
                    .navigate_workspace_id
                    .as_ref()
                    .expect("test precondition")
            )
        );
        preview_key(&mut state, b"\x1b[B");
        assert!(
            state.navigation_target_valid(
                state
                    .navigate_workspace_id
                    .as_ref()
                    .expect("test precondition")
            )
        );
    }
}

#[test]
fn active_preview_is_not_retargeted_by_deletion_or_reboot() {
    for invalidation in ["deleted", "boot", "generation"] {
        let (mut state, _) = state_with_remote();
        let mut local = workspaces(2);
        state.set_endpoint_snapshot_for_generation(
            &ClientEndpointId::Local,
            7,
            Box::new(local.clone()),
        );
        state.set_pane_surface(surface());
        state.compose(100, 28).expect("test precondition");
        enter_navigation(&mut state);
        preview_key(&mut state, b"\x1b[B");
        assert_selected(&state, &ClientEndpointId::Local, "w2");
        let selected = state.navigate_workspace_id.clone();
        match invalidation {
            "boot" => local.boot_id = crate::tests::test_boot_id("new-local-boot"),
            "deleted" => {
                local.revision = local.revision.checked_next().expect("test precondition");
                local.workspaces.pop();
            }
            _ => {}
        }
        let generation = if invalidation == "generation" { 8 } else { 7 };
        state.set_endpoint_snapshot_for_generation(
            &ClientEndpointId::Local,
            generation,
            Box::new(local),
        );
        assert_eq!(state.navigate_workspace_id, selected);
        preview_key(&mut state, b"\r");
        assert_eq!(state.mode, ClientShellMode::Navigate);
        assert!(state.visible_endpoint_notice.is_some());
        for confirm in [false, true] {
            state.config.confirm_close = confirm;
            for key in [b"W", b"D"] {
                preview_key(&mut state, key);
            }
            assert!(state.overlay.is_none());
            assert_eq!(state.mode, ClientShellMode::Navigate);
        }
        assert_eq!(state.workspace_action_id().as_deref(), Some("w1"));
    }
}

#[test]
fn aggregate_navigation_reveals_overflow_and_preserves_order() {
    for compact in [true, false] {
        let (mut state, remote_id) = state_with_remote();
        let mut remote = workspaces(15);
        remote.boot_id = crate::tests::test_boot_id("remote-boot");
        state.set_endpoint_snapshot(&remote_id, Box::new(remote));
        state.sidebar_collapsed = compact;
        state.collapsed_endpoints.insert(remote_id.clone());
        state.compose(100, 18).expect("test precondition");
        enter_navigation(&mut state);
        for number in 1..=15 {
            preview_key(&mut state, b"\x1b[B");
            let id = format!("w{number}");
            assert_selected(&state, &remote_id, &id);
            state.compose(100, 18).expect("test precondition");
            workspace_rect(&state, &remote_id, &id);
        }
        assert!(!state.collapsed_endpoints.contains(&remote_id));
        // Navigation wraps from the last remote workspace back to the first.
        preview_key(&mut state, b"\x1b[B");
        assert_selected(&state, &ClientEndpointId::Local, "w1");
        preview_key(&mut state, b"\x1b[A");
        assert_selected(&state, &remote_id, "w15");
        state.set_endpoint_status(&remote_id, ClientEndpointStatus::Reconnecting);
        preview_key(&mut state, b"\x1b[B");
        assert_selected(&state, &ClientEndpointId::Local, "w1");
    }
}

fn local_navigation_state(compact: bool) -> ClientShellState {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.config.palette = Palette::terminal();
    state.sidebar_collapsed = compact;
    state.set_snapshot(Box::new(workspaces(3)));
    state.set_pane_surface(surface());
    state.compose(100, 28).expect("test precondition");
    state
}

fn request_local_navigation(state: &mut ClientShellState, down: usize) -> String {
    enter_navigation(state);
    for _ in 0..down {
        preview_key(state, b"\x1b[B");
    }
    let outcome = state.handle_input_bytes(b"\r");
    let [ClientShellAction::Endpoint { request, .. }] = outcome.actions.as_slice() else {
        panic!("expected a local workspace focus request");
    };
    assert!(matches!(
        request.method,
        shepr_api::schema::Method::WorkspaceFocus(_)
    ));
    request.id.clone()
}

fn assert_local_highlight(state: &mut ClientShellState, selected_id: &str) {
    let buffer = state
        .compose(100, 28)
        .expect("test precondition")
        .to_ratatui_buffer()
        .expect("test precondition");
    for workspace_id in ["w1", "w2", "w3"] {
        let rect = workspace_rect(state, &ClientEndpointId::Local, workspace_id);
        assert_eq!(
            (rect.x..rect.right())
                .any(|x| buffer[(x, rect.y)].bg == state.config.palette.active_row_bg),
            workspace_id == selected_id,
            "expected only {selected_id} highlighted, checking {workspace_id}"
        );
    }
}

fn set_local_focus(state: &mut ClientShellState, workspace_id: &str, revision: u64) {
    let mut snapshot = workspaces(3);
    snapshot.revision = shepr_protocol::ProjectionRevision::new(revision);
    snapshot.focused_workspace_id = Some(test_workspace_id(workspace_id));
    for workspace in &mut snapshot.workspaces {
        workspace.focused = workspace.workspace_id == workspace_id;
    }
    state.set_snapshot(Box::new(snapshot));
    let mut frame = surface();
    frame.projection_revision = shepr_protocol::ProjectionRevision::new(revision);
    state.set_pane_surface(frame);
}

#[test]
fn accepted_local_navigation_keeps_highlight_until_authoritative_focus() {
    for compact in [false, true] {
        for response_first in [false, true] {
            let mut state = local_navigation_state(compact);
            let request_id = request_local_navigation(&mut state, 2);
            assert_eq!(state.mode, ClientShellMode::Terminal);
            assert!(state.navigate_workspace_id.is_none());
            assert_eq!(
                state
                    .snapshot
                    .as_ref()
                    .expect("test precondition")
                    .focused_workspace_id
                    .as_deref(),
                Some("w1")
            );
            assert_eq!(state.focused_pane_id().as_deref(), Some("w1:p1"));
            assert_local_highlight(&mut state, "w3");
            state.invalidate_pane_surface();
            assert_local_highlight(&mut state, "w3");
            state.set_pane_surface(surface());
            if response_first {
                state.handle_endpoint_result(
                    &crate::tests::test_boot_id("boot-1"),
                    &request_id,
                    Ok(shepr_api::schema::ResponseResult::Ok {}),
                );
                assert_local_highlight(&mut state, "w3");
            }
            set_local_focus(&mut state, "w1", 2);
            assert_local_highlight(&mut state, "w3");
            set_local_focus(&mut state, "w3", 3);
            assert_local_highlight(&mut state, "w3");
            if !response_first {
                state.handle_endpoint_result(
                    &crate::tests::test_boot_id("boot-1"),
                    &request_id,
                    Ok(shepr_api::schema::ResponseResult::Ok {}),
                );
            }
            set_local_focus(&mut state, "w2", 4);
            assert_local_highlight(&mut state, "w2");
        }
    }
}

#[test]
fn failed_local_navigation_releases_only_its_own_highlight() {
    for failure in ["rejected", "endpoint_timeout", "cancelled"] {
        let mut state = local_navigation_state(false);
        let request_id = request_local_navigation(&mut state, 2);
        assert_local_highlight(&mut state, "w3");
        if failure == "cancelled" {
            assert!(state.cancel_endpoint_request(&request_id));
        } else {
            state.handle_endpoint_result(
                &crate::tests::test_boot_id("boot-1"),
                &request_id,
                Err(ClientShellEndpointError {
                    code: Some(failure.into()),
                    message: "focus failed".into(),
                }),
            );
        }
        assert_local_highlight(&mut state, "w1");
        state.handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &request_id,
            Ok(shepr_api::schema::ResponseResult::Ok {}),
        );
        assert_local_highlight(&mut state, "w1");
    }
    for old_down in [1, 2] {
        let mut state = local_navigation_state(false);
        let old_request = request_local_navigation(&mut state, old_down);
        let latest_request = request_local_navigation(&mut state, 2);
        state.cancel_endpoint_request(&old_request);
        assert_local_highlight(&mut state, "w3");
        state.cancel_endpoint_request(&latest_request);
        assert_local_highlight(&mut state, "w1");
    }
}

#[test]
fn pending_navigation_highlight_does_not_survive_identity_changes() {
    for change in ["disconnect", "boot", "generation", "deleted", "endpoint"] {
        let mut state = local_navigation_state(false);
        let request_id = request_local_navigation(&mut state, 2);
        state.handle_endpoint_result(
            &crate::tests::test_boot_id("boot-1"),
            &request_id,
            Ok(shepr_api::schema::ResponseResult::Ok {}),
        );
        assert_local_highlight(&mut state, "w3");
        let mut snapshot = workspaces(3);
        match change {
            "disconnect" => {
                state.mark_endpoint_disconnected(&ClientEndpointId::Local);
                state.set_endpoint_status(&ClientEndpointId::Local, ClientEndpointStatus::Online);
                state.set_snapshot(Box::new(snapshot));
            }
            "boot" => {
                snapshot.boot_id = crate::tests::test_boot_id("replacement-boot");
                state.set_snapshot(Box::new(snapshot));
            }
            "generation" => state.set_endpoint_snapshot_for_generation(
                &ClientEndpointId::Local,
                1,
                Box::new(snapshot),
            ),
            "deleted" => {
                snapshot.workspaces.pop();
                state.set_snapshot(Box::new(snapshot));
                state.set_snapshot(Box::new(workspaces(3)));
            }
            "endpoint" => {
                let machine = remote_machine();
                let remote = ClientEndpointId::Ssh(machine.label.clone());
                state.set_machines(&[machine]);
                state.set_endpoint_status(&remote, ClientEndpointStatus::Online);
                state.set_endpoint_snapshot(&remote, Box::new(snapshot));
                assert!(state.activate_endpoint_projection(&remote));
                assert!(state.pending_workspace_highlight.is_none());
                assert!(state.activate_endpoint_projection(&ClientEndpointId::Local));
            }
            _ => unreachable!(),
        }
        assert!(state.pending_workspace_highlight.is_none(), "{change}");
        let mut frame = surface();
        frame.boot_id = state
            .snapshot
            .as_ref()
            .expect("test precondition")
            .boot_id
            .clone();
        state.set_pane_surface(frame);
        assert_local_highlight(&mut state, "w1");
    }
}

#[test]
fn navigation_highlight_yields_to_new_intent() {
    let mut state = local_navigation_state(false);
    request_local_navigation(&mut state, 2);
    enter_navigation(&mut state);
    preview_key(&mut state, b"\x1b");
    assert_local_highlight(&mut state, "w1");

    request_local_navigation(&mut state, 2);
    let mut unrelated = ClientShellInput::default();
    state.push_endpoint_method(
        shepr_api::schema::Method::SessionSnapshot(shepr_api::schema::EmptyParams::default()),
        &mut unrelated,
    );
    let [ClientShellAction::Endpoint { request, .. }] = unrelated.actions.as_slice() else {
        panic!("expected unrelated request");
    };
    state.cancel_endpoint_request(&request.id);
    assert_local_highlight(&mut state, "w3");
    let mut focus = ClientShellInput::default();
    state.focus_or_activate(
        ClientEndpointId::Local,
        ClientEndpointFocusTarget::Workspace(shepr_test_fixtures::id("w2")),
        &mut focus,
    );
    assert!(state.pending_workspace_highlight.is_none());
}

#[test]
fn directional_pane_focus_releases_an_accepted_workspace_highlight() {
    use shepr_api::schema::{Method, PaneDirection, ResponseResult};

    for (key, direction) in [
        (b'h', PaneDirection::Left),
        (b'j', PaneDirection::Down),
        (b'k', PaneDirection::Up),
        (b'l', PaneDirection::Right),
    ] {
        for rejected in [false, true] {
            let mut state = local_navigation_state(false);
            let pending_request = request_local_navigation(&mut state, 2);
            assert_local_highlight(&mut state, "w3");
            preview_key(&mut state, &[0x02]);
            let outcome = state.handle_input_bytes(&[key]);
            let [ClientShellAction::Endpoint { request, .. }] = outcome.actions.as_slice() else {
                panic!("expected a directional pane focus request");
            };
            let Method::PaneFocusDirection(params) = &request.method else {
                panic!("expected PaneFocusDirection");
            };
            assert_eq!(params.direction, direction);
            assert_eq!(params.pane_id.as_deref(), Some("w1:p1"));
            assert!(state.pending_workspace_highlight.is_none());
            assert_local_highlight(&mut state, "w1");
            let result = if rejected {
                Err(ClientShellEndpointError {
                    code: Some("rejected".into()),
                    message: "focus rejected".into(),
                })
            } else {
                Ok(ResponseResult::Ok {})
            };
            state.handle_endpoint_result(
                &crate::tests::test_boot_id("boot-1"),
                &pending_request,
                result,
            );
            assert_local_highlight(&mut state, "w1");
        }
    }
}

#[test]
fn direct_agent_focus_repaints_when_releasing_a_workspace_highlight() {
    let mut config = Config::default();
    config.keys.focus_agent = shepr_config::BindingConfig::one("ctrl+alt+1");
    let mut projected = workspaces(3);
    projected.agents.push(agent(AgentStatus::Idle, 1));

    for pending in [false, true] {
        let mut state = local_navigation_state(false);
        state.config.keybinds = ClientShellConfig::from_config(&config).keybinds;
        state.set_snapshot(Box::new(projected.clone()));
        state.compose(100, 28).expect("test precondition");
        if pending {
            request_local_navigation(&mut state, 2);
            assert_local_highlight(&mut state, "w3");
        }

        // Direct bindings do not inherit the repaint from leaving prefix mode.
        let outcome = state.handle_raw_events(vec![RawInputEvent::Key(
            shepr_termio::input::TerminalKey::new(
                KeyCode::Char('1'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
        )]);
        assert!(
            matches!(outcome.actions.as_slice(), [ClientShellAction::Endpoint { request, .. }]
            if matches!(&request.method, shepr_api::schema::Method::PaneFocus(params)
                if params.pane_id == "w1:p1"))
        );
        assert!(state.pending_workspace_highlight.is_none());
        assert_eq!(outcome.repaint, pending);
        assert_local_highlight(&mut state, "w1");
    }
}

#[test]
fn cancelled_close_does_not_restore_an_older_navigation_highlight() {
    let mut state = local_navigation_state(false);
    request_local_navigation(&mut state, 2);
    state.mode = ClientShellMode::Navigate;
    state.open_confirm_close_overlay(shepr_test_fixtures::id("w1"));
    state.mode = ClientShellMode::Terminal;
    preview_key(&mut state, b"\x1b");
    assert_eq!(state.mode, ClientShellMode::Navigate);
    preview_key(&mut state, b"\x1b");
    assert_eq!(state.mode, ClientShellMode::Terminal);
    assert_local_highlight(&mut state, "w1");
}

#[test]
fn cancelled_close_returns_to_the_mode_it_was_opened_from() {
    let mut state = local_navigation_state(false);
    assert_eq!(state.mode, ClientShellMode::Terminal);
    state.open_confirm_close_overlay(shepr_test_fixtures::id("w1"));
    preview_key(&mut state, b"\x1b");
    assert!(state.overlay.is_none());
    assert_eq!(state.mode, ClientShellMode::Terminal);
    assert!(state.navigate_workspace_id.is_none());
}

#[test]
fn coalesced_navigation_focus_does_not_leave_a_permanent_highlight() {
    let mut state = local_navigation_state(false);
    let before_request = std::time::Instant::now();
    let request_id = request_local_navigation(&mut state, 2);
    state.handle_endpoint_result(
        &crate::tests::test_boot_id("boot-1"),
        &request_id,
        Ok(shepr_api::schema::ResponseResult::Ok {}),
    );
    // Another client can focus the original workspace before the server projects
    // either change, so a successful request need not produce a new snapshot.
    assert!(!state.tick_workspace_highlight(before_request));
    assert_local_highlight(&mut state, "w3");
    let now = std::time::Instant::now();
    assert!(state.tick_workspace_highlight(now + std::time::Duration::from_secs(2)));
    assert_local_highlight(&mut state, "w1");
    assert!(!state.tick_workspace_highlight(now + std::time::Duration::from_secs(3)));
}

#[test]
fn navigation_highlight_ends_for_noop_focus_and_focused_creation() {
    let mut state = local_navigation_state(false);
    request_local_navigation(&mut state, 0);
    assert!(state.pending_workspace_highlight.is_none());
    set_local_focus(&mut state, "w2", 2);
    assert_local_highlight(&mut state, "w2");

    for focus in [false, true] {
        for method in [
            shepr_api::schema::Method::WorkspaceCreate(shepr_api::schema::WorkspaceCreateParams {
                source_workspace_id: None,
                cwd: None,
                focus,
                label: None,
                env: Default::default(),
            }),
            shepr_api::schema::Method::TabCreate(shepr_api::schema::TabCreateParams {
                workspace_id: Some("w1".into()),
                cwd: None,
                focus,
                label: None,
                env: Default::default(),
            }),
        ] {
            let mut state = local_navigation_state(false);
            request_local_navigation(&mut state, 2);
            let mut outcome = ClientShellInput::default();
            state.push_endpoint_method(method, &mut outcome);
            assert_eq!(state.pending_workspace_highlight.is_none(), focus);
            assert_local_highlight(&mut state, if focus { "w1" } else { "w3" });
        }
    }
}
