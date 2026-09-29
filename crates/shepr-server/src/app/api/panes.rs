use shepr_api::error::{ApiError, ApiErrorCode, ApiResult};

use crate::app::App;
use crate::app::actions::{PaneRemovalCommit, PaneZoomCommand};
use shepr_api::schema::{
    PaneLayoutPane, PaneLayoutRect, PaneLayoutSnapshot, PaneLayoutSplit, PaneReportAgentParams,
    PaneReportAgentSessionParams, ResponseResult,
};
use shepr_core::layout::{NavDirection, PaneId, find_in_direction};
use shepr_protocol::command::{
    EndpointReply, PaneCopyMotion, PaneCopyMotionParams, PaneCopySearchDirection,
    PaneCopySearchParams, PaneDirection, PaneFocusDirectionParams, PaneInputSetParams,
    PaneRenameParams, PaneResizeParams, PaneScrollParams, PaneSelectionReadParams, PaneSplitParams,
    PaneSwapParams, PaneTarget, PaneTextPoint, PaneTextRange, PaneZoomMode, PaneZoomParams,
};

use super::super::api_helpers::{
    detect_state_from_api, normalize_reported_agent_label, pane_in_workspace_not_found,
    pane_not_found, workspace_not_found,
};
use super::EndpointResult;
use super::responses::{failure, success};

mod copy;
mod geometry;
mod reports;

impl App {
    pub(super) fn handle_pane_split(&mut self, params: PaneSplitParams) -> EndpointResult {
        let pane_target = match params.target_pane_id.as_deref() {
            Some(pane_id) => match self.parse_pane_id(pane_id) {
                Some(target) => Some(target),
                None => return Err(pane_not_found(Some(pane_id))),
            },
            None => None,
        };
        let workspace_target = if pane_target.is_none() {
            match params.workspace_id.as_deref() {
                Some(workspace_id) => match self
                    .parse_workspace_id(workspace_id)
                    .filter(|index| self.state.workspaces.get(*index).is_some())
                {
                    Some(index) => Some(index),
                    None => return Err(workspace_not_found(workspace_id)),
                },
                None => None,
            }
        } else {
            None
        };
        let Some(context) = self.state.resolve_pane_context(
            pane_target,
            workspace_target,
            crate::app::actions::PaneContextFallback::ActiveWorkspace,
        ) else {
            let error = match (
                params.target_pane_id.as_deref(),
                params.workspace_id.as_deref(),
            ) {
                (Some(pane_id), _) => pane_not_found(Some(pane_id)),
                (None, Some(workspace_id)) => pane_in_workspace_not_found(workspace_id),
                (None, None) => pane_not_found(None),
            };
            return Err(error);
        };
        let ws_idx = context.workspace_index;
        let target_pane_id = context.pane_id;
        let target_pane_public_id = self.public_pane_id(ws_idx, target_pane_id);
        let extra_env = super::env::normalize_launch_env(params.env)?;
        let geometry = self.state.pane_geometry_for_tab(ws_idx, context.tab_index);
        let split_cwd = match params
            .cwd
            .as_deref()
            .map(super::cwd::launch_cwd)
            .transpose()?
        {
            Some(cwd) => cwd,
            None => {
                let follow_cwd = self.launch_cwd_for_pane_in_workspace(ws_idx, target_pane_id);
                self.resolve_new_terminal_cwd(follow_cwd)
            }
        };
        let default_cwd = self
            .paths
            .current_dir()
            .unwrap_or_else(|| std::path::Path::new("/"))
            .to_path_buf();
        let default_shell = self.state.settings.default_shell.clone();
        let scrollback_limit_bytes = self.state.settings.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let host_terminal_appearance = self.state.host_terminal_appearance;
        let previous_focus = self.state.current_pane_focus_target();
        let spawn = self.pane_spawn_handles();
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return Err(pane_not_found(
                target_pane_public_id
                    .as_deref()
                    .or(params.target_pane_id.as_deref()),
            ));
        };
        let direction = match params.direction {
            shepr_protocol::command::SplitDirection::Right => {
                shepr_core::layout::Direction::Horizontal
            }
            shepr_protocol::command::SplitDirection::Down => {
                shepr_core::layout::Direction::Vertical
            }
        };
        let shell_config =
            shepr_mux::pane::PaneShellConfig::new(&default_shell, self.state.settings.login_shell);
        let split_result = match params.ratio {
            Some(ratio) => ws.split_pane_with_ratio(
                target_pane_id,
                direction,
                ratio,
                &geometry,
                Some(split_cwd),
                default_cwd,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                shell_config,
                extra_env,
                params.focus,
                &spawn,
            ),
            None => ws.split_pane(
                target_pane_id,
                direction,
                &geometry,
                Some(split_cwd),
                default_cwd,
                scrollback_limit_bytes,
                host_terminal_theme,
                host_terminal_appearance,
                shell_config,
                extra_env,
                params.focus,
                &spawn,
            ),
        };
        let (target_tab_idx, new_pane) = match split_result {
            Some(Ok(result)) => result,
            Some(Err(err)) => return failure(ApiErrorCode::PaneSplitFailed, err.to_string()),
            None => {
                return Err(pane_not_found(
                    target_pane_public_id
                        .as_deref()
                        .or(params.target_pane_id.as_deref()),
                ));
            }
        };
        let shepr_mux::workspace::NewPane {
            pane_id,
            terminal,
            runtime,
            prepared_layout,
        } = new_pane;
        let terminal_id = terminal.id.clone();
        let Some(outcome) = self.state.commit_pane_split(
            ws_idx,
            target_tab_idx,
            pane_id,
            prepared_layout,
            terminal,
            params.focus,
            matches!(
                params.right_click,
                shepr_protocol::command::PaneRightClickTarget::Pane
            ),
            previous_focus,
        ) else {
            drop(runtime);
            return failure(
                ApiErrorCode::PaneSplitFailed,
                "split target is no longer available",
            );
        };
        self.terminal_runtimes.insert(terminal_id, runtime);
        self.schedule_session_save();
        let Some(pane) = self.pane_info(outcome.workspace_index, outcome.pane_id) else {
            return failure(ApiErrorCode::PaneSplitFailed, "new pane is unavailable");
        };

        Ok(EndpointReply::PaneInfo {
            pane: Box::new(pane),
        })
    }

    pub(super) fn handle_pane_focus(&mut self, target: &PaneTarget) -> EndpointResult {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&target.pane_id) else {
            return Err(pane_not_found(Some(&target.pane_id)));
        };
        let Some(_tab_idx) = self.tab_index_for_pane(ws_idx, pane_id) else {
            return Err(pane_not_found(Some(&target.pane_id)));
        };

        self.state.focus_pane_in_workspace(ws_idx, pane_id);
        self.state.mode = crate::app::Mode::Terminal;

        let Some(pane) = self.pane_info(ws_idx, pane_id) else {
            return Err(pane_not_found(Some(&target.pane_id)));
        };
        Ok(EndpointReply::PaneInfo {
            pane: Box::new(pane),
        })
    }

    pub(super) fn handle_pane_input_set(&mut self, params: &PaneInputSetParams) -> EndpointResult {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return Err(pane_not_found(Some(&params.pane_id)));
        };
        let Some(pane) = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .and_then(|workspace| workspace.pane_state_mut(pane_id))
        else {
            return Err(pane_not_found(Some(&params.pane_id)));
        };
        pane.right_click_passthrough = matches!(
            params.right_click,
            shepr_protocol::command::PaneRightClickTarget::Pane
        );
        Ok(EndpointReply::Done)
    }

    pub(super) fn handle_pane_rename(&mut self, params: PaneRenameParams) -> EndpointResult {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return Err(pane_not_found(Some(&params.pane_id)));
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.terminal_id(pane_id))
            .cloned()
        else {
            return Err(pane_not_found(Some(&params.pane_id)));
        };
        let Some(terminal) = self.state.terminals.get_mut(&terminal_id) else {
            return Err(pane_not_found(Some(&params.pane_id)));
        };
        match params.label.map(|label| label.trim().to_string()) {
            Some(label) if !label.is_empty() => terminal.set_manual_label(label),
            _ => terminal.clear_manual_label(),
        }
        self.state.mark_session_dirty();
        let Some(pane) = self.pane_info(ws_idx, pane_id) else {
            return Err(pane_not_found(Some(&params.pane_id)));
        };

        Ok(EndpointReply::PaneInfo {
            pane: Box::new(pane),
        })
    }

    pub(super) fn handle_pane_close(&mut self, target: &PaneTarget) -> EndpointResult {
        self.close_pane(target)?;
        Ok(EndpointReply::Done)
    }

    /// Close a pane; errors remain typed until the API response is sent.
    pub(super) fn close_pane(&mut self, target: &PaneTarget) -> Result<(), ApiError> {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&target.pane_id) else {
            return Err(pane_not_found(Some(&target.pane_id)));
        };
        let Some(plan) = self.state.prepare_pane_removal(ws_idx, pane_id) else {
            return Err(pane_not_found(Some(&target.pane_id)));
        };
        let PaneRemovalCommit::Removed(outcome) = self.state.commit_pane_removal(&plan) else {
            return Err(pane_not_found(Some(&target.pane_id)));
        };
        self.shutdown_detached_terminal_runtimes(&outcome.detached_terminal_ids);
        self.schedule_session_save();

        Ok(())
    }
}

fn terminal_word_motion(motion: PaneCopyMotion) -> Option<shepr_mux::pane::TerminalWordMotion> {
    use shepr_mux::pane::TerminalWordMotion;
    match motion {
        PaneCopyMotion::NextWordStart => Some(TerminalWordMotion::NextStart),
        PaneCopyMotion::PreviousWordStart => Some(TerminalWordMotion::PreviousStart),
        PaneCopyMotion::NextWordEnd => Some(TerminalWordMotion::NextEnd),
        PaneCopyMotion::NextBigWordStart => Some(TerminalWordMotion::NextBigStart),
        PaneCopyMotion::PreviousBigWordStart => Some(TerminalWordMotion::PreviousBigStart),
        PaneCopyMotion::NextBigWordEnd => Some(TerminalWordMotion::NextBigEnd),
        PaneCopyMotion::LineEnd
        | PaneCopyMotion::FirstNonBlank
        | PaneCopyMotion::PreviousParagraph
        | PaneCopyMotion::NextParagraph => None,
    }
}

impl App {
    fn resolve_optional_pane(&self, pane_id: Option<&str>) -> Option<(usize, PaneId)> {
        match pane_id {
            Some(pane_id) => self.parse_pane_id(pane_id),
            None => {
                let ws_idx = self.state.active_index()?;
                let pane_id = self.state.workspaces.get(ws_idx)?.focused_pane_id();
                Some((ws_idx, pane_id))
            }
        }
    }

    fn resolve_swap_source(&self, pane_id: Option<&str>) -> Option<(usize, PaneId)> {
        self.resolve_optional_pane(pane_id)
    }

    fn directional_pane_target(
        &self,
        ws_idx: usize,
        tab_idx: usize,
        source_pane_id: PaneId,
        direction: PaneDirection,
    ) -> Option<PaneId> {
        let tab = self.state.workspaces.get(ws_idx)?.tabs().get(tab_idx)?;
        let panes = tab.layout().panes(shepr_mux::workspace::layout_rect(
            self.state.tab_layout_area(ws_idx, tab_idx),
        ));
        let source = panes.iter().find(|pane| pane.id == source_pane_id)?;
        find_in_direction(source, nav_direction(direction), &panes)
    }

    pub(super) fn pane_layout_snapshot(
        &self,
        ws_idx: usize,
        tab_idx: usize,
    ) -> Option<PaneLayoutSnapshot> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab = ws.tabs().get(tab_idx)?;
        let area = self.state.tab_layout_area(ws_idx, tab_idx);
        let layout_area = shepr_mux::workspace::layout_rect(area);
        let focused_pane_id = self.public_pane_id(ws_idx, tab.layout().focused())?;
        // The layout reports what is on screen: a zoomed tab shows only its
        // focused pane over the whole area and no split lines, so its hidden
        // panes and the split tree under them are left out (`zoomed` says the
        // tree exists).
        // `tab_panes` is the one place the zoom rule lives, shared with view
        // computation and spawn sizing.
        let panes = self
            .state
            .pane_geometry_in(area)
            .tab_panes(tab.layout(), tab.zoomed())
            .into_iter()
            .filter_map(|pane| {
                Some(PaneLayoutPane {
                    pane_id: self.public_pane_id(ws_idx, pane.id)?,
                    focused: pane.is_focused,
                    rect: pane_layout_rect(shepr_mux::workspace::layout_rect(pane.rect)),
                })
            })
            .collect();
        let visible_splits = if tab.zoomed() {
            Vec::new()
        } else {
            tab.layout().splits(layout_area)
        };
        let splits = visible_splits
            .into_iter()
            .enumerate()
            .map(|(idx, split)| PaneLayoutSplit {
                id: split_path_id(idx, &split.path),
                direction: match split.direction {
                    shepr_core::layout::Direction::Horizontal => {
                        shepr_api::schema::SplitDirection::Right
                    }
                    shepr_core::layout::Direction::Vertical => {
                        shepr_api::schema::SplitDirection::Down
                    }
                },
                ratio: split.ratio,
                rect: pane_layout_rect(split.area),
            })
            .collect();

        Some(PaneLayoutSnapshot {
            workspace_id: self.public_workspace_id(ws_idx)?,
            tab_id: self.public_tab_id(ws_idx, tab_idx)?,
            zoomed: tab.zoomed(),
            area: pane_layout_rect(layout_area),
            focused_pane_id,
            panes,
            splits,
        })
    }
}

fn nav_direction(direction: PaneDirection) -> NavDirection {
    match direction {
        PaneDirection::Left => NavDirection::Left,
        PaneDirection::Right => NavDirection::Right,
        PaneDirection::Up => NavDirection::Up,
        PaneDirection::Down => NavDirection::Down,
    }
}

fn pane_layout_rect(rect: shepr_core::geometry::Rect) -> PaneLayoutRect {
    PaneLayoutRect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

fn split_path_id(idx: usize, path: &[shepr_core::geometry::SplitBranch]) -> String {
    if path.is_empty() {
        return format!("split_{idx}_root");
    }
    let path = path
        .iter()
        .map(|branch| {
            if *branch == shepr_core::geometry::SplitBranch::Second {
                "1"
            } else {
                "0"
            }
        })
        .collect::<Vec<_>>()
        .join("");
    format!("split_{idx}_{path}")
}

fn invalid_agent() -> ApiResult {
    failure(ApiErrorCode::InvalidAgent, "agent label must not be empty")
}

#[cfg(test)]
mod tests;
