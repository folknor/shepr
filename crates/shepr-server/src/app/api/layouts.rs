use crate::app::App;
use shepr_protocol::command::LayoutSetSplitRatioParams;

use super::endpoint::{Handled, HandlerResult};

impl App {
    /// Sets one split's ratio. Moves nobody: a client changing the layout of a
    /// workspace it views stays where it is.
    pub(super) fn handle_layout_set_split_ratio(
        &mut self,
        params: &LayoutSetSplitRatioParams,
    ) -> HandlerResult {
        let workspace_id = self.endpoint_workspace(&params.workspace_id)?;
        // The path was read at the epoch the client drew; a topology change
        // since then may have put another split at it.
        if self
            .state
            .workspace(&workspace_id)
            .is_none_or(|workspace| workspace.tree().layout_epoch() != params.epoch)
        {
            return Err(shepr_protocol::command::EndpointError::SplitGone.into());
        }
        let path = shepr_core::layout::SplitPath::from(params.path.clone());
        let outcome = self
            .state
            .edit_workspace_geometry(&workspace_id, |workspace| {
                workspace.set_split_ratio(&path, params.ratio)
            });
        let effects = outcome.into();
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

    fn app_with_workspace() -> crate::app::TestApp {
        let mut app = App::new(&ServerConfig::default());
        app.state
            .test_set_workspaces(vec![Workspace::test_new("layout")]);
        app
    }

    fn params(app: &App, ratio: shepr_core::layout::SplitRatio) -> LayoutSetSplitRatioParams {
        LayoutSetSplitRatioParams {
            workspace_id: app.state.ws(0).id(),
            path: Vec::new(),
            epoch: app.state.ws(0).tree().layout_epoch(),
            ratio,
        }
    }

    #[test]
    fn layout_set_split_ratio_updates_existing_split() {
        let mut app = app_with_workspace();
        let root = app.state.ws(0).tree().root();
        app.state.test_split_workspace(0, Direction::Horizontal);
        assert!(app.state.ws_mut(0).focus_pane(root));

        let request = params(
            &app,
            shepr_core::layout::SplitRatio::new(0.72).expect("test ratio is valid"),
        );
        let handled = app
            .handle_layout_set_split_ratio(&request)
            .expect("the ratio is set");

        assert_eq!(handled.navigate, None);
        let splits = app
            .state
            .ws(0)
            .tree()
            .layout()
            .splits(shepr_core::geometry::Rect::new(0, 0, 100, 20));
        assert_eq!(splits.len(), 1);
        assert!((splits[0].ratio.get() - 0.72).abs() < f32::EPSILON);
        assert_eq!(app.state.ws(0).tree().focused(), root);
    }

    #[test]
    fn stale_epoch_split_command_refuses_a_replacement_at_the_same_path() {
        let mut app = app_with_workspace();
        app.state.test_split_workspace(0, Direction::Horizontal);
        let stale = params(
            &app,
            shepr_core::layout::SplitRatio::new(0.72).expect("test ratio is valid"),
        );
        // A second client splits a child, advancing the layout epoch.
        app.state.test_split_workspace(0, Direction::Vertical);
        let area = shepr_core::geometry::Rect::new(0, 0, 100, 20);
        let before = app
            .state
            .ws(0)
            .tree()
            .layout()
            .splits(area)
            .iter()
            .map(|split| split.ratio)
            .collect::<Vec<_>>();
        assert!(app.handle_layout_set_split_ratio(&stale).is_err());
        assert_eq!(
            app.state
                .ws(0)
                .tree()
                .layout()
                .splits(area)
                .iter()
                .map(|split| split.ratio)
                .collect::<Vec<_>>(),
            before
        );
    }

    #[test]
    fn layout_set_split_ratio_on_a_missing_split_changes_nothing() {
        let mut app = app_with_workspace();

        let missing_params = params(
            &app,
            shepr_core::layout::SplitRatio::new(0.72).expect("test ratio is valid"),
        );
        app.handle_layout_set_split_ratio(&missing_params)
            .expect("a current epoch is not refused");
        assert!(
            app.state
                .ws(0)
                .tree()
                .layout()
                .splits(shepr_core::geometry::Rect::new(0, 0, 100, 20))
                .is_empty()
        );
    }
}
