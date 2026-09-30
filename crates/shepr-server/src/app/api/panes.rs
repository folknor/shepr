use shepr_api::error::{ApiErrorCode, ApiResult};

use crate::app::actions::PaneRemovalCommit;
use crate::app::{App, EndpointContext};
use shepr_api::schema::{PaneReportAgentParams, PaneReportAgentSessionParams, ResponseResult};
use shepr_core::layout::{NavDirection, PaneId, find_in_direction};
use shepr_protocol::command::{
    EndpointReply, PaneCopyMotion, PaneCopyMotionParams, PaneCopySearchDirection,
    PaneCopySearchParams, PaneDirection, PaneFocusDirectionParams, PaneInputSetParams,
    PaneLineMotion, PaneParagraphMotion, PaneRenameParams, PaneResizeParams, PaneScrollParams,
    PaneSelectionReadParams, PaneSplitParams, PaneSwapParams, PaneTarget, PaneTextPoint,
    PaneTextRange, PaneWordMotion, PaneZoomParams,
};

use super::super::api_helpers::{
    detect_state_from_api, normalize_reported_agent_label, pane_not_found,
};
use super::endpoint::{Handled, HandlerResult, pane_missing, rejected};
use super::responses::{failure, success};

mod copy;
mod geometry;
mod reports;

impl App {
    /// Splits the target pane, spawns a shell in the new pane and moves the
    /// requester onto the workspace with the new pane focused. The new pane
    /// is sized against the workspace's recorded geometry; the requester's own
    /// geometry is used only for a workspace with none recorded.
    pub(super) fn handle_pane_split(
        &mut self,
        params: &PaneSplitParams,
        ctx: &EndpointContext,
    ) -> HandlerResult {
        let (ws_idx, target_pane_id) = self.endpoint_pane(&params.pane_id)?;
        let workspace_id = params.pane_id.workspace_id().clone();
        let geometry = self
            .state
            .workspace_spawn_geometry(ws_idx)
            .or(ctx.requester_geometry)
            .unwrap_or_else(|| self.headless_spawn_geometry());
        let chrome = self.state.pane_geometry_in(geometry.area);
        let follow_cwd = self.launch_cwd_for_pane_in_workspace(ws_idx, target_pane_id);
        let split_cwd = self.resolve_new_terminal_cwd(follow_cwd);
        let default_cwd = self
            .paths
            .current_dir()
            .unwrap_or_else(|| std::path::Path::new("/"))
            .to_path_buf();
        let default_shell = self.state.settings.default_shell.clone();
        let scrollback_limit_bytes = self.state.settings.pane_scrollback_limit_bytes;
        let host_terminal_theme = self.state.host_terminal_theme;
        let host_terminal_appearance = self.state.host_terminal_appearance;
        let spawn = self.pane_spawn_handles();
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return Err(pane_missing(&params.pane_id));
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
        let split_result = ws.split_pane(
            target_pane_id,
            direction,
            &chrome,
            geometry.cell_px(),
            Some(split_cwd),
            default_cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            host_terminal_appearance,
            shell_config,
            true,
            &spawn,
        );
        let new_pane = match split_result {
            Some(Ok(result)) => result,
            Some(Err(err)) => {
                return rejected(format!("the pane could not be split: {err}"));
            }
            None => return Err(pane_missing(&params.pane_id)),
        };
        let shepr_mux::workspace::NewPane {
            pane_id,
            terminal,
            runtime,
            prepared_layout,
        } = new_pane;
        let terminal_id = terminal.id.clone();
        let Some(outcome) =
            self.state
                .commit_pane_split(ws_idx, pane_id, prepared_layout, terminal)
        else {
            drop(runtime);
            return rejected("the split target is no longer available");
        };
        self.terminal_runtimes.insert(terminal_id, runtime);
        self.schedule_session_save();
        let Some(pane) = self.pane_info(outcome.workspace_index, outcome.pane_id) else {
            return rejected("the new pane is unavailable");
        };

        Handled::navigating(
            EndpointReply::PaneInfo {
                pane: Box::new(pane),
            },
            workspace_id,
        )
    }

    /// Focuses the pane and moves the requester onto its workspace, even when
    /// the pane already holds focus.
    pub(super) fn handle_pane_focus(&mut self, target: &PaneTarget) -> HandlerResult {
        let (ws_idx, pane_id) = self.endpoint_pane(&target.pane_id)?;
        self.state.focus_pane_in_workspace(ws_idx, pane_id);

        let Some(pane) = self.pane_info(ws_idx, pane_id) else {
            return Err(pane_missing(&target.pane_id));
        };
        Handled::navigating(
            EndpointReply::PaneInfo {
                pane: Box::new(pane),
            },
            target.pane_id.workspace_id().clone(),
        )
    }

    pub(super) fn handle_pane_input_set(&mut self, params: &PaneInputSetParams) -> HandlerResult {
        let (ws_idx, pane_id) = self.endpoint_pane(&params.pane_id)?;
        let Some(pane) = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .and_then(|workspace| workspace.pane_state_mut(pane_id))
        else {
            return Err(pane_missing(&params.pane_id));
        };
        pane.right_click_passthrough = matches!(
            params.right_click,
            shepr_protocol::command::PaneRightClickTarget::Pane
        );
        Handled::done()
    }

    pub(super) fn handle_pane_rename(&mut self, params: PaneRenameParams) -> HandlerResult {
        let (ws_idx, pane_id) = self.endpoint_pane(&params.pane_id)?;
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.terminal_id(pane_id))
            .cloned()
        else {
            return Err(pane_missing(&params.pane_id));
        };
        let Some(terminal) = self.state.terminals.get_mut(&terminal_id) else {
            return Err(pane_missing(&params.pane_id));
        };
        match params.label.map(|label| label.trim().to_string()) {
            Some(label) if !label.is_empty() => terminal.set_manual_label(label),
            _ => terminal.clear_manual_label(),
        }
        self.state.mark_session_dirty();
        let Some(pane) = self.pane_info(ws_idx, pane_id) else {
            return Err(pane_missing(&params.pane_id));
        };

        Handled::reply(EndpointReply::PaneInfo {
            pane: Box::new(pane),
        })
    }

    pub(super) fn handle_pane_close(&mut self, target: &PaneTarget) -> HandlerResult {
        let (ws_idx, pane_id) = self.endpoint_pane(&target.pane_id)?;
        let Some(plan) = self.state.prepare_pane_removal(ws_idx, pane_id) else {
            return Err(pane_missing(&target.pane_id));
        };
        let PaneRemovalCommit::Removed(outcome) = self.state.commit_pane_removal(&plan) else {
            return Err(pane_missing(&target.pane_id));
        };
        self.shutdown_detached_terminal_runtimes(&outcome.detached_terminal_ids);
        self.schedule_session_save();

        Handled::done()
    }
}

fn terminal_word_motion(motion: PaneWordMotion) -> shepr_mux::pane::TerminalWordMotion {
    use shepr_mux::pane::TerminalWordMotion;
    match motion {
        PaneWordMotion::NextStart => TerminalWordMotion::NextStart,
        PaneWordMotion::PreviousStart => TerminalWordMotion::PreviousStart,
        PaneWordMotion::NextEnd => TerminalWordMotion::NextEnd,
        PaneWordMotion::NextBigStart => TerminalWordMotion::NextBigStart,
        PaneWordMotion::PreviousBigStart => TerminalWordMotion::PreviousBigStart,
        PaneWordMotion::NextBigEnd => TerminalWordMotion::NextBigEnd,
    }
}

impl App {
    fn directional_pane_target(
        &self,
        ws_idx: usize,
        source_pane_id: PaneId,
        direction: PaneDirection,
    ) -> Option<PaneId> {
        let workspace = self.state.workspaces.get(ws_idx)?;
        let panes = workspace.layout().panes(shepr_mux::workspace::layout_rect(
            self.state.workspace_layout_area(ws_idx),
        ));
        let source = panes.iter().find(|pane| pane.id == source_pane_id)?;
        find_in_direction(source, nav_direction(direction), &panes)
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

fn invalid_agent() -> ApiResult {
    failure(ApiErrorCode::InvalidAgent, "agent label must not be empty")
}

#[cfg(test)]
mod tests;
