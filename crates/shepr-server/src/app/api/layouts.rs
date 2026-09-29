use shepr_api::error::{ApiErrorCode, ApiResult};

use crate::app::App;
use shepr_api::schema::{
    LayoutDescription, LayoutNode, LayoutPane, LayoutSetSplitRatioParams, ResponseResult,
    SplitDirection,
};
use shepr_core::layout::{Direction, Node, PaneId};

use super::responses::{failure, success};

impl App {
    pub(super) fn handle_layout_set_split_ratio(
        &mut self,
        params: &LayoutSetSplitRatioParams,
    ) -> ApiResult {
        if !params.ratio.is_finite() {
            return failure(ApiErrorCode::InvalidRatio, "ratio must be finite");
        }
        let Some((ws_idx, tab_idx)) =
            self.resolve_layout_target(params.tab_id.as_deref(), params.pane_id.as_deref())
        else {
            return failure(ApiErrorCode::LayoutNotFound, "layout target not found");
        };

        // The API spells a split path as booleans: `true` descends into the
        // second branch.
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
            .is_some_and(|ws| ws.set_tab_split_ratio_at(tab_idx, &path, params.ratio));
        if !changed {
            return failure(ApiErrorCode::SplitNotFound, "split path not found");
        }

        self.schedule_session_save();
        let Some(layout) = self.layout_description(ws_idx, tab_idx) else {
            return failure(ApiErrorCode::LayoutNotFound, "layout unavailable");
        };
        success(ResponseResult::LayoutSplitRatioSet { layout })
    }

    /// The tab a layout request addresses: the tab named by `tab_id`, the tab
    /// holding `pane_id`, or the active tab when neither is given. Naming both
    /// resolves to nothing.
    fn resolve_layout_target(
        &self,
        tab_id: Option<&str>,
        pane_id: Option<&str>,
    ) -> Option<(usize, usize)> {
        match (tab_id, pane_id) {
            (Some(_), Some(_)) => None,
            (Some(tab_id), None) => self.parse_tab_id(tab_id),
            (None, Some(pane_id)) => {
                let (ws_idx, pane_id) = self.parse_pane_id(pane_id)?;
                let tab_idx = self
                    .state
                    .workspaces
                    .get(ws_idx)?
                    .find_tab_index_for_pane(pane_id)?;
                Some((ws_idx, tab_idx))
            }
            (None, None) => {
                let ws_idx = self.state.active_index()?;
                let tab_idx = self.state.workspaces.get(ws_idx)?.active_tab_index();
                Some((ws_idx, tab_idx))
            }
        }
    }

    fn layout_description(&self, ws_idx: usize, tab_idx: usize) -> Option<LayoutDescription> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab = ws.tabs().get(tab_idx)?;
        Some(LayoutDescription {
            workspace_id: self.public_workspace_id(ws_idx)?,
            tab_id: self.public_tab_id(ws_idx, tab_idx)?,
            zoomed: tab.zoomed(),
            focused_pane_id: self.public_pane_id(ws_idx, tab.layout().focused())?,
            root: self.layout_node_description(ws_idx, tab_idx, tab.layout().root())?,
        })
    }

    fn layout_node_description(
        &self,
        ws_idx: usize,
        tab_idx: usize,
        node: &Node,
    ) -> Option<LayoutNode> {
        match node {
            Node::Pane(pane_id) => Some(LayoutNode::Pane {
                pane: self.layout_pane_description(ws_idx, tab_idx, *pane_id)?,
            }),
            Node::Split {
                direction,
                ratio,
                first,
                second,
            } => Some(LayoutNode::Split {
                direction: match direction {
                    Direction::Horizontal => SplitDirection::Right,
                    Direction::Vertical => SplitDirection::Down,
                },
                ratio: ratio.get(),
                first: Box::new(self.layout_node_description(ws_idx, tab_idx, first)?),
                second: Box::new(self.layout_node_description(ws_idx, tab_idx, second)?),
            }),
        }
    }

    fn layout_pane_description(
        &self,
        ws_idx: usize,
        tab_idx: usize,
        pane_id: PaneId,
    ) -> Option<LayoutPane> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab = ws.tabs().get(tab_idx)?;
        let terminal_id = tab.terminal_id(pane_id)?;
        let terminal = self.state.terminals.get(terminal_id);
        Some(LayoutPane {
            pane_id: Some(self.public_pane_id(ws_idx, pane_id)?.to_string()),
            label: terminal.and_then(|terminal| terminal.manual_label.clone()),
            cwd: tab
                .cwd_for_pane(pane_id, &self.state.terminals, &self.terminal_runtimes)
                .map(|cwd| cwd.display().to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use shepr_api::schema::{ErrorResponse, SuccessResponse};
    use shepr_config::Config;
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
        let root = app.state.workspaces[0].tabs()[0].root_pane();
        let right = app.state.workspaces[0].test_split(Direction::Horizontal);
        app.state.ensure_test_terminals();
        assert!(app.state.workspaces[0].focus_pane_in_tab(0, root));
        let right_terminal_id = app.state.workspaces[0].tabs()[0]
            .terminal_id(right)
            .cloned()
            .expect("test precondition");
        app.state
            .terminals
            .get_mut(&right_terminal_id)
            .expect("test precondition")
            .set_manual_label("tests".into());

        let response = app.handle_layout_set_split_ratio(&LayoutSetSplitRatioParams {
            tab_id: None,
            pane_id: None,
            path: vec![],
            ratio: 0.72,
        });

        let success: SuccessResponse = crate::test_support::test_success(&response);
        let ResponseResult::LayoutSplitRatioSet { layout } = success.result else {
            panic!("expected layout split ratio set response");
        };
        assert_eq!(
            layout.workspace_id,
            app.public_workspace_id(0).expect("test precondition")
        );
        assert_eq!(
            layout.focused_pane_id,
            app.public_pane_id(0, root).expect("test precondition")
        );
        let LayoutNode::Split {
            direction,
            ratio,
            second,
            ..
        } = layout.root
        else {
            panic!("expected split layout root");
        };
        assert_eq!(direction, SplitDirection::Right);
        assert!((ratio - 0.72).abs() < f32::EPSILON);
        let LayoutNode::Pane { pane } = *second else {
            panic!("expected second pane");
        };
        assert_eq!(pane.label.as_deref(), Some("tests"));
        assert_eq!(
            pane.pane_id,
            Some(app.public_pane_id(0, right).expect("test precondition")).map(|id| id.to_string())
        );
    }

    #[test]
    fn layout_set_split_ratio_rejects_missing_split() {
        let mut app = app_with_workspace();

        let response = app.handle_layout_set_split_ratio(&LayoutSetSplitRatioParams {
            tab_id: None,
            pane_id: None,
            path: vec![],
            ratio: 0.72,
        });

        let error: ErrorResponse = crate::test_support::test_error(&response);
        assert_eq!(error.error.code, "split_not_found");
    }
}
