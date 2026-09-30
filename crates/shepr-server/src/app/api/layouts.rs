use shepr_api::error::ApiErrorCode;

use crate::app::App;
use shepr_protocol::command::{EndpointReply, LayoutSetSplitRatioParams};

use super::EndpointResult;
use super::responses::failure;

impl App {
    pub(super) fn handle_layout_set_split_ratio(
        &mut self,
        params: &LayoutSetSplitRatioParams,
    ) -> EndpointResult {
        if !params.ratio.is_finite() {
            return failure(ApiErrorCode::InvalidRatio, "ratio must be finite");
        }
        let Some(ws_idx) =
            self.resolve_layout_target(params.workspace_id.as_deref(), params.pane_id.as_deref())
        else {
            return failure(ApiErrorCode::LayoutNotFound, "layout target not found");
        };

        // A split path is spelled as booleans: `true` descends into the second
        // branch.
        let path = params
            .path
            .iter()
            .map(|&second| {
                if second {
                    shepr_core::geometry::SplitBranch::Second
                } else {
                    shepr_core::geometry::SplitBranch::First
                }
            })
            .collect::<Vec<_>>();
        let changed = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .is_some_and(|ws| ws.set_split_ratio_at(&path, params.ratio));
        if !changed {
            return failure(ApiErrorCode::SplitNotFound, "split path not found");
        }

        self.schedule_session_save();
        Ok(EndpointReply::Done)
    }

    /// The workspace a layout request addresses: the workspace named by
    /// `workspace_id`, the workspace holding `pane_id`, or the active
    /// workspace when neither is given. Naming both resolves to nothing.
    fn resolve_layout_target(
        &self,
        workspace_id: Option<&str>,
        pane_id: Option<&str>,
    ) -> Option<usize> {
        match (workspace_id, pane_id) {
            (Some(_), Some(_)) => None,
            (Some(workspace_id), None) => self.parse_workspace_id(workspace_id),
            (None, Some(pane_id)) => {
                let (ws_idx, _) = self.parse_pane_id(pane_id)?;
                Some(ws_idx)
            }
            (None, None) => self.state.active_index(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use shepr_config::Config;
    use shepr_core::layout::Direction;
    use shepr_mux::workspace::Workspace;

    fn app_with_workspace() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), crate::app::AppPolicy::Test, api_rx);
        app.state.workspaces = vec![Workspace::test_new("layout")];
        app.state.set_active_index(Some(0));
        app.state.set_selected_index(Some(0));
        app.state.ensure_test_terminals();
        app
    }

    #[test]
    fn layout_set_split_ratio_updates_existing_split() {
        let mut app = app_with_workspace();
        let root = app.state.workspaces[0].root_pane();
        app.state.workspaces[0].test_split(Direction::Horizontal);
        app.state.ensure_test_terminals();
        assert!(app.state.workspaces[0].focus_pane(root));

        let response = app.handle_layout_set_split_ratio(&LayoutSetSplitRatioParams {
            workspace_id: None,
            pane_id: None,
            path: vec![],
            ratio: 0.72,
        });

        assert_eq!(response, Ok(EndpointReply::Done));
        let splits = app.state.workspaces[0]
            .layout()
            .splits(shepr_core::geometry::Rect::new(0, 0, 100, 20));
        assert_eq!(splits.len(), 1);
        assert!((splits[0].ratio - 0.72).abs() < f32::EPSILON);
        assert_eq!(app.state.workspaces[0].layout().focused(), root);
    }

    #[test]
    fn layout_set_split_ratio_rejects_missing_split() {
        let mut app = app_with_workspace();

        let response = app.handle_layout_set_split_ratio(&LayoutSetSplitRatioParams {
            workspace_id: None,
            pane_id: None,
            path: vec![],
            ratio: 0.72,
        });

        assert_eq!(
            response
                .expect_err("a one-pane workspace has no split")
                .code,
            ApiErrorCode::SplitNotFound
        );
    }
}
