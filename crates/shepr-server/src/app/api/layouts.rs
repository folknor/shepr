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
        if !params.ratio.is_finite() {
            return rejected("ratio must be finite");
        }
        let ws_idx = self.endpoint_workspace(&params.workspace_id)?;
        let area = shepr_mux::workspace::layout_rect(self.state.workspace_layout_area(ws_idx));
        let Some(current_ratio) = self.state.workspaces.get(ws_idx).and_then(|workspace| {
            workspace
                .layout()
                .splits(area)
                .into_iter()
                .find(|split| split.path == params.path)
                .map(|split| split.ratio)
        }) else {
            return rejected("split path not found");
        };
        let next_ratio = shepr_core::layout::SplitRatio::clamped(params.ratio).get();
        // Both sides went through the same clamp, so a repeat of the stored
        // ratio is bit-for-bit equal.
        let changed = current_ratio.to_bits() != next_ratio.to_bits();
        if changed {
            let set = self
                .state
                .workspaces
                .get_mut(ws_idx)
                .is_some_and(|workspace| workspace.set_split_ratio_at(&params.path, params.ratio));
            if !set {
                return rejected("split path not found");
            }
            self.schedule_session_save();
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
    use shepr_config::Config;
    use shepr_core::layout::Direction;
    use shepr_mux::workspace::Workspace;
    use shepr_protocol::command::EndpointError;

    fn app_with_workspace() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), crate::app::AppPolicy::Test, api_rx);
        app.state.workspaces = vec![Workspace::test_new("layout")];
        app.state.ensure_test_terminals();
        app
    }

    fn params(app: &App, ratio: f32) -> LayoutSetSplitRatioParams {
        LayoutSetSplitRatioParams {
            workspace_id: app.public_workspace_id(0).expect("test precondition"),
            path: vec![],
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
            .handle_layout_set_split_ratio(&params(&app, 0.72))
            .expect("the ratio is set");

        assert_eq!(handled.navigate, None);
        let splits = app.state.workspaces[0]
            .layout()
            .splits(shepr_core::geometry::Rect::new(0, 0, 100, 20));
        assert_eq!(splits.len(), 1);
        assert!((splits[0].ratio - 0.72).abs() < f32::EPSILON);
        assert_eq!(app.state.workspaces[0].layout().focused(), root);
    }

    #[test]
    fn layout_set_split_ratio_rejects_missing_split_and_bad_ratios() {
        let mut app = app_with_workspace();

        let missing_params = params(&app, 0.72);
        let missing = app
            .handle_layout_set_split_ratio(&missing_params)
            .expect_err("a one-pane workspace has no split");
        assert_eq!(
            missing.error,
            EndpointError::Rejected("split path not found".into())
        );
        let bad_ratio_params = params(&app, f32::NAN);
        let bad_ratio = app
            .handle_layout_set_split_ratio(&bad_ratio_params)
            .expect_err("a non-finite ratio is refused");
        assert_eq!(
            bad_ratio.error,
            EndpointError::Rejected("ratio must be finite".into())
        );
    }
}
