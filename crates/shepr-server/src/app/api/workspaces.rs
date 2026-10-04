use crate::app::{App, EndpointContext};
use shepr_protocol::command::{
    EndpointError, EndpointReply, WorkspaceCloseParams, WorkspaceCreateParams,
    WorkspaceCreateSource, WorkspaceMoveParams, WorkspaceRenameParams, WorkspaceTarget,
};

use shepr_mux::terminal::Label;

use super::endpoint::{
    EndpointEffects, Handled, HandlerError, HandlerResult, internal_with_effects, workspace_missing,
};

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
                match self.state.workspace(workspace_id) {
                    Some(_) => self.resolved_new_workspace_cwd(workspace_id),
                    None => self.resolve_new_terminal_cwd(None),
                }
            }
            WorkspaceCreateSource::Default => self.resolve_new_terminal_cwd(None),
        };
        let geometry = ctx
            .requester_geometry
            .unwrap_or_else(|| self.headless_spawn_geometry());
        let outcome = self
            .create_workspace_outcome(&cwd, geometry)
            .map_err(|err| {
                EndpointError::ResourceFailure(format!("the workspace could not be created: {err}"))
            })?;
        let workspace_id = outcome.workspace_id;
        // A workspace created without a name, or with a blank one, keeps the
        // name of its directory that it was given at creation.
        if let Some(label) = params.label.and_then(Label::new)
            && let Some(workspace) = self.state.workspaces.get_mut(&workspace_id)
        {
            workspace.set_name(label);
            crate::logging::workspace_renamed(&workspace.id());
        }
        let effects = EndpointEffects::from(&outcome);
        if self.state.workspace(&workspace_id).is_none() {
            return internal_with_effects("the new workspace is unavailable", effects);
        }
        Handled::navigating_with_effects(EndpointReply::Done, workspace_id, effects)
    }

    /// Moves the requester onto the workspace, even when it already views it.
    pub(super) fn handle_workspace_focus(&mut self, target: &WorkspaceTarget) -> HandlerResult {
        let id = self.endpoint_workspace(&target.workspace_id)?;
        let Some(workspace) = self.workspace_info(&id) else {
            return Err(workspace_missing(&target.workspace_id).into());
        };
        Handled::navigating(
            EndpointReply::WorkspaceInfo { workspace },
            target.workspace_id,
        )
    }

    pub(super) fn handle_workspace_rename(
        &mut self,
        params: WorkspaceRenameParams,
    ) -> HandlerResult {
        let id = self.endpoint_workspace(&params.workspace_id)?;
        // A blank name names the workspace after its current directory.
        let name = match params.label.and_then(Label::new) {
            Some(name) => name,
            None => {
                let workspace = self
                    .state
                    .workspace(&id)
                    .ok_or_else(|| workspace_missing(&params.workspace_id))?;
                Label::for_directory(
                    workspace
                        .resolved_identity_cwd(&self.terminal_runtimes)
                        .as_path(),
                )
            }
        };
        let outcome = self
            .state
            .rename_workspace(&id, name)
            .ok_or_else(|| workspace_missing(&params.workspace_id))?;
        let effects = outcome.into();
        let Some(workspace) = self.workspace_info(&id) else {
            return Err(HandlerError {
                error: workspace_missing(&params.workspace_id),
                effects,
            });
        };

        Handled::reply_with_effects(EndpointReply::WorkspaceInfo { workspace }, effects)
    }

    pub(super) fn handle_workspace_move(&mut self, params: &WorkspaceMoveParams) -> HandlerResult {
        self.endpoint_workspace(&params.workspace_id)?;
        // The anchor is a stable id, resolved against the live order by the
        // set, never a client's old slot.
        if let Some(anchor) = &params.before_workspace_id {
            self.endpoint_workspace(anchor)?;
        }
        let outcome = self
            .state
            .move_workspace(&params.workspace_id, params.before_workspace_id.as_ref());
        let effects = outcome.into();
        Handled::done_with_effects(effects)
    }

    pub(super) fn handle_workspace_close(
        &mut self,
        params: &WorkspaceCloseParams,
    ) -> HandlerResult {
        let id = self.endpoint_workspace(&params.workspace_id)?;
        let effects = if let Some(outcome) = self.state.close_workspace(&id) {
            self.shutdown_detached_pane_runtimes(&outcome.removed);
            EndpointEffects::from(&outcome)
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

    fn app() -> crate::app::TestApp {
        let mut app = App::new(&ServerConfig::default());
        app.set_test_shell(super::super::test_support::exiting_test_command());
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
        app.state
            .test_set_workspaces(vec![Workspace::test_new("spaces")]);
        let followed = app.state.ws(0).id();

        // The split pane becomes the focused pane, away from the root pane.
        let root_public = app
            .state
            .pane(app.state.ws(0).tree().root())
            .expect("test precondition")
            .public_id();
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
        let ws = app.state.ws(0);
        let root_cwd = ws.identity_cwd().to_path_buf();
        let focused_pane = ws.tree().focused();
        assert_ne!(focused_pane, ws.tree().root());
        app.state
            .terminal_mut(focused_pane)
            .set_cwd(shepr_mux::UsableCwd::new(focused_cwd.clone()).expect("test cwd is usable"));

        let handled = app
            .handle_workspace_create(
                create(WorkspaceCreateSource::Follow(followed)),
                &EndpointContext::without_geometry(),
            )
            .expect("the workspace is created");

        assert_eq!(handled.reply, EndpointReply::Done);
        let created_cwd = app.state.ws(1).identity_cwd();
        assert_eq!(canonical(created_cwd), canonical(&focused_cwd));
        assert_ne!(canonical(created_cwd), canonical(&root_cwd));
        shutdown_test_runtimes(&mut app);
    }

    #[tokio::test]
    async fn workspace_create_sources_pick_the_cwd_and_a_vanished_follow_falls_back() {
        use super::super::test_support::shutdown_test_runtimes;

        let mut app = app();
        app.state.test_set_workspaces(vec![
            Workspace::test_new("first"),
            Workspace::test_new("source"),
        ]);
        // The bookmark is on another workspace: creation follows the named one.
        app.state.seed_bookmark_index(Some(0));
        shutdown_test_runtimes(&mut app);

        let source_scratch = crate::test_support::ScratchDir::new("ws-source");
        let source_cwd = source_scratch.to_path_buf();
        let pane_id = app.state.ws(1).tree().focused();
        app.state
            .terminal_mut(pane_id)
            .set_cwd(shepr_mux::UsableCwd::new(source_cwd.clone()).expect("test cwd is usable"));
        let source_workspace_id = app.state.ws(1).id();
        let ctx = EndpointContext::without_geometry();

        let followed = app
            .handle_workspace_create(
                create(WorkspaceCreateSource::Follow(source_workspace_id)),
                &ctx,
            )
            .expect("follow creates");
        assert_eq!(
            followed.navigate,
            Some(app.state.ws(2).id()),
            "creation navigates the requester to the new workspace"
        );
        assert_eq!(
            canonical(app.state.ws(2).identity_cwd()),
            canonical(&source_cwd)
        );

        // A workspace that vanished falls back to the default cwd rather than
        // failing the creation.
        let vanished = WorkspaceId::from_number(999).expect("nonzero number");
        let default_cwd = app.resolve_new_terminal_cwd(None);
        app.handle_workspace_create(create(WorkspaceCreateSource::Follow(vanished)), &ctx)
            .expect("a vanished follow falls back to the default");
        assert_eq!(
            canonical(app.state.ws(3).identity_cwd()),
            canonical(&default_cwd)
        );

        app.handle_workspace_create(create(WorkspaceCreateSource::Default), &ctx)
            .expect("default creates");
        assert_eq!(
            canonical(app.state.ws(4).identity_cwd()),
            canonical(&default_cwd)
        );

        let captured = app
            .handle_workspace_create(
                create(WorkspaceCreateSource::Cwd(source_cwd.clone().into())),
                &ctx,
            )
            .expect("an explicit cwd creates");
        assert_eq!(captured.reply, EndpointReply::Done);
        assert_eq!(
            canonical(app.state.ws(5).identity_cwd()),
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
            area: shepr_core::geometry::Rect::new(0, 0, 100, 30),
            cell: shepr_core::geometry::CellPx::new(9, 18),
        };

        let handled = app
            .handle_workspace_create(
                create(WorkspaceCreateSource::Default),
                &EndpointContext {
                    requester_geometry: Some(geometry),
                },
            )
            .expect("the workspace is created");

        let workspace = app.state.ws(0);
        assert_eq!(handled.navigate, Some(workspace.id()));
        let runtime = app.test_runtime(workspace.tree().root());
        assert_eq!(
            runtime.grid_size(),
            shepr_core::geometry::GridSize::clamped(100, 30)
        );
        assert_eq!(
            runtime
                .read()
                .pixel_mouse()
                .extent()
                .map(|extent| (extent.width().get(), extent.height().get())),
            Some((100 * 9, 30 * 18)),
            "the first window size already has pixel dimensions"
        );
        // Recorded at creation, before any geometry pass has run.
        assert_eq!(app.state.ws(0).spawn_geometry(), Some(geometry));
        shutdown_test_runtimes(&mut app);
    }

    #[test]
    fn workspace_move_reorders_workspaces_and_moves_nobody() {
        let mut app = app();
        app.state.test_set_workspaces(vec![
            Workspace::test_new("one"),
            Workspace::test_new("two"),
            Workspace::test_new("three"),
        ]);
        let moved_id = app.state.ws(0).id();

        let handled = app
            .handle_workspace_move(&WorkspaceMoveParams {
                workspace_id: moved_id,
                before_workspace_id: None,
            })
            .expect("the move succeeds");

        assert_eq!(handled.reply, EndpointReply::Done);
        assert_eq!(handled.navigate, None);
        assert_eq!(app.state.ws(2).id(), moved_id);
        assert_eq!(app.state.ws(2).name(), "one");
    }

    #[test]
    fn workspace_move_noop_leaves_order_unchanged_and_missing_anchor_is_refused() {
        let mut app = app();
        app.state
            .test_set_workspaces(vec![Workspace::test_new("one"), Workspace::test_new("two")]);
        let moved_id = app.state.ws(0).id();

        let anchor_id = app.state.ws(1).id();
        app.handle_workspace_move(&WorkspaceMoveParams {
            workspace_id: moved_id,
            before_workspace_id: Some(anchor_id),
        })
        .expect("a no-op move succeeds");
        assert_eq!(app.state.ws(0).id(), moved_id);
        assert_eq!(app.state.ws(0).name(), "one");

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
        app.state.test_set_workspaces(vec![
            Workspace::test_new("source"),
            Workspace::test_new("middle"),
            Workspace::test_new("anchor"),
            Workspace::test_new("last"),
        ]);
        let command = WorkspaceMoveParams {
            workspace_id: app.state.ws(0).id(),
            before_workspace_id: Some(app.state.ws(2).id()),
        };
        // Another client moves the anchor after the first client's snapshot.
        let anchor = app.state.ws(2).id();
        assert!(app.state.move_workspace(&anchor, None).changed());
        app.handle_workspace_move(&command)
            .expect("live anchor resolves");
        assert_eq!(app.state.ws(2).name(), "source");
        assert_eq!(app.state.ws(3).name(), "anchor");
        // Another client then closes the anchor. No other workspace substitutes.
        app.state.close_workspace(&anchor);
        let order = app
            .state
            .workspaces
            .iter()
            .map(|ws| ws.name().to_owned())
            .collect::<Vec<_>>();
        assert!(app.handle_workspace_move(&command).is_err());
        assert_eq!(
            app.state
                .workspaces
                .iter()
                .map(|ws| ws.name().to_owned())
                .collect::<Vec<_>>(),
            order
        );
    }

    #[test]
    fn workspace_close_removes_the_workspace_with_all_its_panes() {
        let mut app = app();
        let mut closing = Workspace::test_new("closing");
        closing.test_split(shepr_core::layout::Direction::Horizontal);
        app.state
            .test_set_workspaces(vec![closing, Workspace::test_new("survivor")]);
        let workspace_id = app.state.ws(0).id();

        let handled = app
            .handle_workspace_close(&WorkspaceCloseParams { workspace_id })
            .expect("the workspace closes");

        assert_eq!(handled.reply, EndpointReply::Done);
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.ws(0).name(), "survivor");
    }

    #[test]
    fn workspace_focus_navigates_on_success_even_with_nothing_to_mutate() {
        let mut app = app();
        app.state
            .test_set_workspaces(vec![Workspace::test_new("one"), Workspace::test_new("two")]);
        app.state.session_dirty = false;
        let target = app.state.ws(1).id();

        let handled = app
            .handle_workspace_focus(&WorkspaceTarget {
                workspace_id: target,
            })
            .expect("the workspace is focused");

        assert_eq!(handled.navigate.as_ref(), Some(&target));
        let EndpointReply::WorkspaceInfo { workspace } = handled.reply else {
            panic!("expected workspace info");
        };
        assert_eq!(workspace.workspace_id, target);
        assert!(!app.state.session_dirty, "nothing was mutated");

        // The same again, as when the requester already views it.
        let again = app
            .handle_workspace_focus(&WorkspaceTarget {
                workspace_id: target,
            })
            .expect("focusing again succeeds");
        assert_eq!(again.navigate.as_ref(), Some(&target));
        assert!(matches!(
            again.reply,
            EndpointReply::WorkspaceInfo { workspace } if workspace.workspace_id == target
        ));
    }

    #[test]
    fn a_gone_workspace_is_refused_by_every_workspace_command() {
        let mut app = app();
        app.state
            .test_set_workspaces(vec![Workspace::test_new("only")]);
        let gone = WorkspaceId::from_number(99).expect("nonzero number");
        let refusal = EndpointError::WorkspaceGone(gone);

        assert_eq!(
            app.handle_workspace_focus(&WorkspaceTarget { workspace_id: gone })
                .expect_err("the workspace is gone")
                .error,
            refusal
        );
        assert_eq!(
            app.handle_workspace_rename(WorkspaceRenameParams {
                workspace_id: gone,
                label: Some("x".into()),
            })
            .expect_err("the workspace is gone")
            .error,
            refusal
        );
        assert_eq!(
            app.handle_workspace_move(&WorkspaceMoveParams {
                workspace_id: gone,
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
    fn workspace_info_for_a_closed_workspace_is_none() {
        let mut app = app();
        app.state
            .test_set_workspaces(vec![Workspace::test_new("one"), Workspace::test_new("two")]);
        let closed = app.state.ws(1).id();
        let open = app.state.ws(0).id();
        assert!(app.workspace_info(&closed).is_some());

        app.state.close_workspace(&closed);

        assert!(app.workspace_info(&open).is_some());
        assert!(app.workspace_info(&closed).is_none());
    }

    #[test]
    fn workspace_rename_uses_the_shared_dirty_schedule() {
        let mut app = app();
        app.persist();
        app.state
            .test_set_workspaces(vec![Workspace::test_new("before")]);
        let workspace_id = app.state.ws(0).id();
        let sample = crate::app::AppClock {
            now: app.clock.now + std::time::Duration::from_secs(2),
            wall_now: app.clock.wall_now,
        };
        app.set_clock(sample);

        app.handle_workspace_rename(WorkspaceRenameParams {
            workspace_id,
            label: Some("after".into()),
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
