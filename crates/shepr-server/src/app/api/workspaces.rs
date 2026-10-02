use crate::app::{App, EndpointContext};
use shepr_protocol::command::{
    EndpointError, EndpointReply, WorkspaceCloseParams, WorkspaceCreateParams,
    WorkspaceCreateSource, WorkspaceMoveParams, WorkspaceRenameParams, WorkspaceTarget,
};

use super::endpoint::{
    EndpointEffects, Handled, HandlerError, HandlerResult, rejected_with_effects, workspace_missing,
};

/// A workspace label as the server stores it: trimmed, and an empty one
/// clears the custom name so the automatic label returns, as a pane rename does.
fn normalized_workspace_label(label: String) -> Option<String> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        None
    } else if trimmed.len() == label.len() {
        Some(label)
    } else {
        Some(trimmed.to_owned())
    }
}

impl App {
    /// Creates a workspace and moves the requester onto it. Its first pane
    /// spawns at the requester's geometry (area and cell size), which is then
    /// the workspace's recorded geometry.
    pub(super) fn handle_workspace_create(
        &mut self,
        params: WorkspaceCreateParams,
        ctx: &EndpointContext,
    ) -> HandlerResult {
        let cwd = match &params.source {
            WorkspaceCreateSource::Cwd(raw) => super::cwd::launch_cwd(raw)?,
            // A workspace that vanished since the client chose it falls back to
            // the default, like the client with none to follow.
            WorkspaceCreateSource::Follow(workspace_id) => {
                match self.resolve_workspace_id(workspace_id) {
                    Some(index) => self.resolved_new_workspace_cwd(index),
                    None => self.resolve_new_terminal_cwd(None),
                }
            }
            WorkspaceCreateSource::Default => self.resolve_new_terminal_cwd(None),
        };
        let geometry = ctx
            .requester_geometry
            .unwrap_or_else(|| self.headless_spawn_geometry());
        let index = self.create_workspace(&cwd, geometry).map_err(|err| {
            EndpointError::Rejected(format!("the workspace could not be created: {err}"))
        })?;
        if let Some(label) = params.label.and_then(normalized_workspace_label)
            && let Some(workspace) = self.state.workspaces.get_mut(index)
        {
            workspace.set_custom_name(label);
            crate::logging::workspace_renamed(&workspace.id);
        }
        let effects = EndpointEffects {
            shell_projection_changed: true,
            pane_surface_changed: true,
            layout_changed: true,
            workspace_membership_changed: true,
            ..EndpointEffects::default()
        };
        let Some(workspace_id) = self.public_workspace_id(index) else {
            return rejected_with_effects("the new workspace is unavailable", effects);
        };
        Handled::navigating_with_effects(EndpointReply::Done, workspace_id, effects)
    }

    /// Moves the requester onto the workspace, even when it already views it.
    pub(super) fn handle_workspace_focus(&mut self, target: &WorkspaceTarget) -> HandlerResult {
        let index = self.endpoint_workspace(&target.workspace_id)?;
        let Some(workspace) = self.workspace_info(index) else {
            return Err(workspace_missing(&target.workspace_id).into());
        };
        Handled::navigating(
            EndpointReply::WorkspaceInfo { workspace },
            target.workspace_id.clone(),
        )
    }

    pub(super) fn handle_workspace_rename(
        &mut self,
        params: WorkspaceRenameParams,
    ) -> HandlerResult {
        let index = self.endpoint_workspace(&params.workspace_id)?;
        let Some(ws) = self.state.workspaces.get_mut(index) else {
            return Err(workspace_missing(&params.workspace_id).into());
        };
        let label = normalized_workspace_label(params.label);
        let changed = ws.custom_name != label;
        if changed {
            ws.custom_name = label;
            crate::logging::workspace_renamed(&ws.id);
            self.state.mark_session_dirty();
        }
        let effects = EndpointEffects {
            shell_projection_changed: changed,
            ..EndpointEffects::default()
        };
        let Some(workspace) = self.workspace_info(index) else {
            return Err(HandlerError {
                error: workspace_missing(&params.workspace_id),
                effects,
            });
        };

        Handled::reply_with_effects(EndpointReply::WorkspaceInfo { workspace }, effects)
    }

    pub(super) fn handle_workspace_move(&mut self, params: &WorkspaceMoveParams) -> HandlerResult {
        let index = self.endpoint_workspace(&params.workspace_id)?;
        // Resolve the anchor against the live order, never a client's old slot.
        let insert_index = match &params.before_workspace_id {
            Some(anchor) => self.endpoint_workspace(anchor)?,
            None => self.state.workspaces.len(),
        };
        let changed = self.state.move_workspace(index, insert_index);
        let effects = if changed {
            EndpointEffects {
                shell_projection_changed: true,
                workspace_order_changed: true,
                ..EndpointEffects::default()
            }
        } else {
            EndpointEffects::default()
        };
        Handled::done_with_effects(effects)
    }

    pub(super) fn handle_workspace_close(
        &mut self,
        params: &WorkspaceCloseParams,
    ) -> HandlerResult {
        let index = self.endpoint_workspace(&params.workspace_id)?;
        let effects = if let Some(outcome) = self.state.close_workspace_at(index) {
            self.shutdown_detached_terminal_runtimes(&outcome.detached_terminal_ids);
            EndpointEffects {
                shell_projection_changed: true,
                pane_surface_changed: true,
                layout_changed: true,
                workspace_membership_changed: true,
                ..EndpointEffects::default()
            }
        } else {
            EndpointEffects::default()
        };
        Handled::done_with_effects(effects)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::SpawnGeometry;
    use crate::test_support::*;
    use shepr_config::ServerConfig;
    use shepr_mux::workspace::Workspace;
    use shepr_protocol::WorkspaceId;
    use shepr_termio::host_term::cell_size::HostCellSize;

    fn app() -> App {
        let mut app = App::new(&ServerConfig::default(), crate::app::AppPolicy::Test);
        app.state.settings.default_shell =
            super::super::test_support::exiting_test_command().into();
        app.state.settings.login_shell = false;
        app
    }

    fn create(source: WorkspaceCreateSource) -> WorkspaceCreateParams {
        WorkspaceCreateParams {
            source,
            label: None,
        }
    }

    fn canonical(path: &std::path::Path) -> std::path::PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    // `new_cwd = follow` must anchor on the focused pane for a creation that
    // follows a workspace: the focused pane's cwd, not the root pane's.
    #[tokio::test]
    async fn workspace_create_follows_the_focused_pane_cwd_not_the_root_pane() {
        use super::super::test_support::shutdown_test_runtimes;

        let mut app = app();
        app.state.workspaces = vec![Workspace::test_new("spaces")];
        app.state.ensure_test_terminals();
        let followed = app.state.workspaces[0].id.clone();

        // The split pane becomes the focused pane, away from the root pane.
        let root_public = app
            .public_pane_id(0, app.state.workspaces[0].root_pane())
            .expect("test precondition");
        app.handle_pane_split(
            &shepr_protocol::command::PaneSplitParams {
                pane_id: root_public,
                direction: shepr_protocol::command::SplitDirection::Right,
            },
            &EndpointContext::without_geometry(),
        )
        .expect("the pane is split");
        // Drop runtimes so cwd resolution deterministically uses cached state.
        shutdown_test_runtimes(&mut app);

        let focused_scratch = crate::test_support::ScratchDir::new("ws-follow");
        let focused_cwd = focused_scratch.to_path_buf();
        let ws = &app.state.workspaces[0];
        let root_cwd = ws.identity_cwd.clone();
        let focused_pane = ws.focused_pane_id();
        assert_ne!(focused_pane, ws.root_pane());
        let terminal_id = ws
            .terminal_id(focused_pane)
            .cloned()
            .expect("test precondition");
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .set_cwd(shepr_mux::UsableCwd::new(focused_cwd.clone()).expect("test cwd is usable"));

        let handled = app
            .handle_workspace_create(
                create(WorkspaceCreateSource::Follow(followed)),
                &EndpointContext::without_geometry(),
            )
            .expect("the workspace is created");

        assert_eq!(handled.reply, EndpointReply::Done);
        let created_cwd = &app.state.workspaces[1].identity_cwd;
        assert_eq!(canonical(created_cwd), canonical(&focused_cwd));
        assert_ne!(canonical(created_cwd), canonical(&root_cwd));
        shutdown_test_runtimes(&mut app);
    }

    #[tokio::test]
    async fn workspace_create_sources_pick_the_cwd_and_a_vanished_follow_falls_back() {
        use super::super::test_support::shutdown_test_runtimes;

        let mut app = app();
        app.state.workspaces = vec![Workspace::test_new("first"), Workspace::test_new("source")];
        app.state.ensure_test_terminals();
        // The bookmark is on another workspace: creation follows the named one.
        app.state.set_bookmark_index(Some(0));
        shutdown_test_runtimes(&mut app);

        let source_scratch = crate::test_support::ScratchDir::new("ws-source");
        let source_cwd = source_scratch.to_path_buf();
        let pane_id = app.state.workspaces[1].focused_pane_id();
        let terminal_id = app.state.workspaces[1]
            .terminal_id(pane_id)
            .cloned()
            .expect("test precondition");
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test precondition")
            .set_cwd(shepr_mux::UsableCwd::new(source_cwd.clone()).expect("test cwd is usable"));
        let source_workspace_id = app.public_workspace_id(1).expect("test precondition");
        let ctx = EndpointContext::without_geometry();

        let followed = app
            .handle_workspace_create(
                create(WorkspaceCreateSource::Follow(source_workspace_id)),
                &ctx,
            )
            .expect("follow creates");
        assert_eq!(
            followed.navigate,
            app.public_workspace_id(2),
            "creation navigates the requester to the new workspace"
        );
        assert_eq!(
            canonical(&app.state.workspaces[2].identity_cwd),
            canonical(&source_cwd)
        );

        // A workspace that vanished falls back to the default cwd rather than
        // failing the creation.
        let vanished = WorkspaceId::from_number(999).expect("nonzero number");
        let default_cwd = app.resolve_new_terminal_cwd(None);
        app.handle_workspace_create(create(WorkspaceCreateSource::Follow(vanished)), &ctx)
            .expect("a vanished follow falls back to the default");
        assert_eq!(
            canonical(&app.state.workspaces[3].identity_cwd),
            canonical(&default_cwd)
        );

        app.handle_workspace_create(create(WorkspaceCreateSource::Default), &ctx)
            .expect("default creates");
        assert_eq!(
            canonical(&app.state.workspaces[4].identity_cwd),
            canonical(&default_cwd)
        );

        let captured = app
            .handle_workspace_create(
                create(WorkspaceCreateSource::Cwd(source_cwd.display().to_string())),
                &ctx,
            )
            .expect("an explicit cwd creates");
        assert_eq!(captured.reply, EndpointReply::Done);
        assert_eq!(
            canonical(&app.state.workspaces[5].identity_cwd),
            canonical(&source_cwd)
        );
        shutdown_test_runtimes(&mut app);
    }

    #[tokio::test]
    async fn a_new_workspaces_first_pty_size_carries_the_requesters_cell_size() {
        use super::super::test_support::shutdown_test_runtimes;

        let mut app = app();
        app.state.settings.pane_borders = shepr_config::PaneBordersConfig::Off;
        app.state.settings.pane_scrollbars = false;
        let geometry = SpawnGeometry {
            area: ratatui::layout::Rect::new(0, 0, 100, 30),
            cell_size: HostCellSize {
                width_px: 9,
                height_px: 18,
            },
        };

        let handled = app
            .handle_workspace_create(
                create(WorkspaceCreateSource::Default),
                &EndpointContext {
                    requester_geometry: Some(geometry),
                },
            )
            .expect("the workspace is created");

        let workspace = &app.state.workspaces[0];
        assert_eq!(handled.navigate.as_ref(), Some(&workspace.id));
        let runtime = app.test_runtime(workspace.root_pane());
        assert_eq!(
            runtime.grid_size(),
            shepr_core::geometry::GridSize::clamped(100, 30)
        );
        assert_eq!(
            runtime.pixel_size(),
            Some((100 * 9, 30 * 18)),
            "the first window size already has pixel dimensions"
        );
        // Recorded at creation, before any geometry pass has run.
        assert_eq!(app.state.workspace_spawn_geometry(0), Some(geometry));
        shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn workspace_move_reorders_workspaces_and_moves_nobody() {
        let mut app = app();
        app.state.workspaces = vec![
            Workspace::test_new("one"),
            Workspace::test_new("two"),
            Workspace::test_new("three"),
        ];
        let moved_id = app.public_workspace_id(0).expect("test precondition");

        let handled = app
            .handle_workspace_move(&WorkspaceMoveParams {
                workspace_id: moved_id.clone(),
                before_workspace_id: None,
            })
            .expect("the move succeeds");

        assert_eq!(handled.reply, EndpointReply::Done);
        assert_eq!(handled.navigate, None);
        assert_eq!(
            app.public_workspace_id(2).expect("test precondition"),
            moved_id
        );
        assert_eq!(app.state.workspaces[2].display_name(), "one");
    }

    #[test]
    fn workspace_move_noop_leaves_order_unchanged_and_missing_anchor_is_refused() {
        let mut app = app();
        app.state.workspaces = vec![Workspace::test_new("one"), Workspace::test_new("two")];
        let moved_id = app.public_workspace_id(0).expect("test precondition");

        app.handle_workspace_move(&WorkspaceMoveParams {
            workspace_id: moved_id.clone(),
            before_workspace_id: app.public_workspace_id(1),
        })
        .expect("a no-op move succeeds");
        assert_eq!(
            app.public_workspace_id(0).expect("test precondition"),
            moved_id
        );
        assert_eq!(app.state.workspaces[0].display_name(), "one");

        let missing = shepr_protocol::WorkspaceId::from_number(usize::MAX).expect("nonzero");
        assert!(
            app.handle_workspace_move(&WorkspaceMoveParams {
                workspace_id: moved_id,
                before_workspace_id: Some(missing),
            })
            .is_err()
        );
    }

    #[test]
    fn stale_client_workspace_move_follows_live_anchor_and_refuses_deleted_anchor() {
        let mut app = app();
        app.state.workspaces = vec![
            Workspace::test_new("source"),
            Workspace::test_new("middle"),
            Workspace::test_new("anchor"),
            Workspace::test_new("last"),
        ];
        let command = WorkspaceMoveParams {
            workspace_id: app.public_workspace_id(0).expect("source"),
            before_workspace_id: app.public_workspace_id(2),
        };
        // Another client moves the anchor after the first client's snapshot.
        assert!(app.state.move_workspace(2, 4));
        app.handle_workspace_move(&command)
            .expect("live anchor resolves");
        assert_eq!(app.state.workspaces[2].display_name(), "source");
        assert_eq!(app.state.workspaces[3].display_name(), "anchor");
        // Another client then closes the anchor. No other workspace substitutes.
        app.state.workspaces.remove(3);
        let order = app
            .state
            .workspaces
            .iter()
            .map(|ws| ws.display_name().clone())
            .collect::<Vec<_>>();
        assert!(app.handle_workspace_move(&command).is_err());
        assert_eq!(
            app.state
                .workspaces
                .iter()
                .map(|ws| ws.display_name().clone())
                .collect::<Vec<_>>(),
            order
        );
    }

    #[test]
    fn workspace_close_removes_the_workspace_with_all_its_panes() {
        let mut app = app();
        let mut closing = Workspace::test_new("closing");
        closing.test_split(shepr_core::layout::Direction::Horizontal);
        app.state.workspaces = vec![closing, Workspace::test_new("survivor")];
        app.state.ensure_test_terminals();
        let workspace_id = app.public_workspace_id(0).expect("test precondition");

        let handled = app
            .handle_workspace_close(&WorkspaceCloseParams { workspace_id })
            .expect("the workspace closes");

        assert_eq!(handled.reply, EndpointReply::Done);
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.workspaces[0].display_name(), "survivor");
    }

    #[test]
    fn workspace_focus_navigates_on_success_even_with_nothing_to_mutate() {
        let mut app = app();
        app.state.workspaces = vec![Workspace::test_new("one"), Workspace::test_new("two")];
        app.state.session_dirty = false;
        let target = app.public_workspace_id(1).expect("test precondition");

        let handled = app
            .handle_workspace_focus(&WorkspaceTarget {
                workspace_id: target.clone(),
            })
            .expect("the workspace is focused");

        assert_eq!(handled.navigate.as_ref(), Some(&target));
        let EndpointReply::WorkspaceInfo { workspace } = handled.reply else {
            panic!("expected workspace info");
        };
        assert_eq!(workspace.workspace_id, target);
        assert!(!workspace.focused, "the server loop fills the flag in");
        assert!(!app.state.session_dirty, "nothing was mutated");

        // The same again, as when the requester already views it.
        let again = app
            .handle_workspace_focus(&WorkspaceTarget {
                workspace_id: target.clone(),
            })
            .expect("focusing again succeeds");
        assert_eq!(again.navigate.as_ref(), Some(&target));

        // Filling the flag follows the requester's location.
        let mut reply = again.reply;
        app.fill_reply_focus(&mut reply, Some(&target));
        assert!(matches!(
            &reply,
            EndpointReply::WorkspaceInfo { workspace } if workspace.focused
        ));
        let other = app.public_workspace_id(0);
        app.fill_reply_focus(&mut reply, other.as_ref());
        assert!(matches!(
            &reply,
            EndpointReply::WorkspaceInfo { workspace } if !workspace.focused
        ));
    }

    #[test]
    fn a_gone_workspace_is_refused_by_every_workspace_command() {
        let mut app = app();
        app.state.workspaces = vec![Workspace::test_new("only")];
        let gone = WorkspaceId::from_number(99).expect("nonzero number");
        let refusal = EndpointError::Rejected(format!("workspace {gone} not found"));

        assert_eq!(
            app.handle_workspace_focus(&WorkspaceTarget {
                workspace_id: gone.clone()
            })
            .expect_err("the workspace is gone")
            .error,
            refusal
        );
        assert_eq!(
            app.handle_workspace_rename(WorkspaceRenameParams {
                workspace_id: gone.clone(),
                label: "x".into(),
            })
            .expect_err("the workspace is gone")
            .error,
            refusal
        );
        assert_eq!(
            app.handle_workspace_move(&WorkspaceMoveParams {
                workspace_id: gone.clone(),
                before_workspace_id: None,
            })
            .expect_err("the workspace is gone")
            .error,
            refusal
        );
        assert_eq!(
            app.handle_workspace_close(&WorkspaceCloseParams { workspace_id: gone })
                .expect_err("the workspace is gone")
                .error,
            refusal
        );
    }

    #[test]
    fn workspace_info_for_a_stale_index_is_none() {
        let mut app = app();
        app.state.workspaces = vec![Workspace::test_new("one")];

        assert!(app.workspace_info(0).is_some());
        assert!(app.workspace_info(1).is_none());
    }

    #[test]
    fn workspace_rename_uses_the_shared_dirty_schedule() {
        let mut app = app();
        app.persist_for_test();
        app.state.workspaces = vec![Workspace::test_new("before")];
        let workspace_id = app.public_workspace_id(0).expect("test precondition");
        let sample = crate::app::AppClock {
            now: app.clock.now + std::time::Duration::from_secs(2),
            wall_now: app.clock.wall_now,
        };
        app.set_clock(sample);

        app.handle_workspace_rename(WorkspaceRenameParams {
            workspace_id,
            label: "after".into(),
        })
        .expect("the workspace is renamed");

        assert!(app.state.session_dirty);
        assert_eq!(app.session_saver.autosave_deadline(), None);
        app.sync_session_save_schedule();
        assert!(!app.state.session_dirty);
        assert_eq!(
            app.session_saver.autosave_deadline(),
            Some(sample.now + crate::limits::SESSION_SAVE_DEBOUNCE)
        );
    }
}
