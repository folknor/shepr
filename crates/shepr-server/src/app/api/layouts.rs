use crate::app::App;
use shepr_protocol::command::LayoutSetSplitRatioParams;

use super::endpoint::{EndpointEffects, Handled, HandlerResult, rejected};

impl App {
    /// Sets one split's ratio. Moves nobody: a client changing the layout of a
    /// workspace it views stays where it is.
    pub(super) fn handle_layout_set_split_ratio(
        &mut self,
        params: &LayoutSetSplitRatioParams,
    ) -> HandlerResult {
        let ws_idx = self.endpoint_workspace(&params.workspace_id)?;
        let resolve_children = |ids: &[shepr_protocol::PublicPaneId]| {
            ids.iter()
                .map(|id| {
                    let (workspace, pane) = self.endpoint_pane(id)?;
                    if workspace != ws_idx {
                        return Err(shepr_protocol::command::EndpointError::Rejected(
                            "split pane belongs to another workspace".into(),
                        ));
                    }
                    Ok(pane)
                })
                .collect::<Result<Vec<_>, _>>()
        };
        let first = resolve_children(&params.first_panes)?;
        let second = resolve_children(&params.second_panes)?;
        let Some(path) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.layout().split_path_for_children(&first, &second))
        else {
            return rejected("split children not found");
        };
        let changed = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .is_some_and(|workspace| workspace.set_split_ratio_at(&path, params.ratio));
        if changed {
            self.state.mark_session_dirty();
        }
        let effects = if changed {
            EndpointEffects {
                pane_surface_changed: true,
                layout_changed: true,
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
    use crate::test_support::*;
    use shepr_config::ServerConfig;
    use shepr_core::layout::Direction;
    use shepr_mux::workspace::Workspace;
    use shepr_protocol::command::EndpointError;

    fn app_with_workspace() -> App {
        let mut app = App::new(&ServerConfig::default(), crate::app::AppPolicy::Test);
        app.state.workspaces = vec![Workspace::test_new("layout")];
        app.state.ensure_test_terminals();
        app
    }

    fn params(app: &App, ratio: shepr_core::layout::SplitRatio) -> LayoutSetSplitRatioParams {
        LayoutSetSplitRatioParams {
            workspace_id: app.public_workspace_id(0).expect("test precondition"),
            first_panes: app.state.workspaces[0]
                .layout()
                .panes(shepr_core::geometry::Rect::new(0, 0, 100, 20))
                .first()
                .and_then(|pane| app.public_pane_id(0, pane.id))
                .into_iter()
                .collect(),
            second_panes: app.state.workspaces[0]
                .layout()
                .panes(shepr_core::geometry::Rect::new(0, 0, 100, 20))
                .get(1)
                .and_then(|pane| app.public_pane_id(0, pane.id))
                .into_iter()
                .collect(),
            ratio,
        }
    }

    #[test]
    fn layout_set_split_ratio_updates_existing_split() {
        let mut app = app_with_workspace();
        let root = app.state.workspaces[0].root_pane();
        app.state.workspaces[0].test_split(Direction::Horizontal);
        app.state.ensure_test_terminals();
        assert!(app.state.workspaces[0].focus_pane(root));

        let handled = app
            .handle_layout_set_split_ratio(&params(
                &app,
                shepr_core::layout::SplitRatio::new(0.72).expect("test ratio is valid"),
            ))
            .expect("the ratio is set");

        assert_eq!(handled.navigate, None);
        let splits = app.state.workspaces[0]
            .layout()
            .splits(shepr_core::geometry::Rect::new(0, 0, 100, 20));
        assert_eq!(splits.len(), 1);
        assert!((splits[0].ratio.get() - 0.72).abs() < f32::EPSILON);
        assert_eq!(app.state.workspaces[0].layout().focused(), root);
    }

    #[test]
    fn stale_client_split_command_refuses_a_replacement_at_the_same_path() {
        let mut app = app_with_workspace();
        app.state.workspaces[0].test_split(Direction::Horizontal);
        app.state.ensure_test_terminals();
        let stale = params(
            &app,
            shepr_core::layout::SplitRatio::new(0.72).expect("test ratio is valid"),
        );
        // A second client splits a child, replacing the old root's membership.
        app.state.workspaces[0].test_split(Direction::Vertical);
        app.state.ensure_test_terminals();
        let area = shepr_core::geometry::Rect::new(0, 0, 100, 20);
        let before = app.state.workspaces[0]
            .layout()
            .splits(area)
            .iter()
            .map(|split| split.ratio)
            .collect::<Vec<_>>();
        assert!(app.handle_layout_set_split_ratio(&stale).is_err());
        assert_eq!(
            app.state.workspaces[0]
                .layout()
                .splits(area)
                .iter()
                .map(|split| split.ratio)
                .collect::<Vec<_>>(),
            before
        );
    }

    #[test]
    fn layout_set_split_ratio_rejects_missing_split() {
        let mut app = app_with_workspace();

        let missing_params = params(
            &app,
            shepr_core::layout::SplitRatio::new(0.72).expect("test ratio is valid"),
        );
        let missing = app
            .handle_layout_set_split_ratio(&missing_params)
            .expect_err("a one-pane workspace has no split");
        assert_eq!(
            missing.error,
            EndpointError::Rejected("split children not found".into())
        );
    }
}
