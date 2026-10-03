use shepr_api::error::{ApiError, ApiErrorCode};

use crate::app::actions::PaneRemovalCommit;
use crate::app::{App, EndpointContext};
use shepr_api::schema::{PaneReportAgentParams, PaneReportAgentSessionParams, ResponseResult};
use shepr_core::layout::{NavDirection, PaneId, find_in_direction};
use shepr_protocol::command::{
    EndpointReply, PaneCopyMotion, PaneCopyMotionParams, PaneCopySearchDirection,
    PaneCopySearchParams, PaneDirection, PaneFocusDirectionParams, PaneInputSetParams,
    PaneLineMotion, PaneParagraphMotion, PaneRenameParams, PaneResizeParams, PaneScrollParams,
    PaneSelectionReadParams, PaneSplitParams, PaneSwapParams, PaneTarget, PaneTextRange,
    PaneWordMotion, PaneZoomParams,
};

use super::super::api_helpers::{detect_state_from_api, normalized_user_label, pane_not_found};
use super::endpoint::{
    EndpointEffects, Handled, HandlerError, HandlerResult, pane_missing, rejected,
    rejected_with_effects,
};
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
        let Some(ws) = self.state.workspaces.get(ws_idx) else {
            return Err(pane_missing(&params.pane_id).into());
        };
        let direction = match params.direction {
            shepr_protocol::command::SplitDirection::Right => {
                shepr_core::layout::Direction::Horizontal
            }
            shepr_protocol::command::SplitDirection::Down => {
                shepr_core::layout::Direction::Vertical
            }
        };
        let Some(prepared) = ws.prepare_split(
            target_pane_id,
            direction,
            &chrome,
            geometry.cell_px(),
            split_cwd,
            true,
        ) else {
            return Err(pane_missing(&params.pane_id).into());
        };
        let public_number = prepared.public_id.number();
        let runtime = match self.launch_pane(
            prepared.pane_id,
            prepared.public_id,
            prepared.geometry,
            prepared.terminal.cwd(),
            shepr_mux::pane::LaunchKind::Fresh,
        ) {
            Ok(runtime) => runtime,
            Err(err) => return rejected(format!("the pane could not be split: {err}")),
        };
        let shepr_mux::workspace::PreparedSplit {
            pane_id,
            terminal,
            prepared_layout,
            ..
        } = prepared;
        let terminal_id = terminal.id.clone();
        let Some(outcome) =
            self.state
                .commit_pane_split(ws_idx, pane_id, prepared_layout, terminal, public_number)
        else {
            drop(runtime);
            return rejected("the split target is no longer available");
        };
        self.install_terminal_runtime(terminal_id, runtime);
        let effects = EndpointEffects::from(&outcome);
        let Some(pane) = self.pane_info(outcome.workspace_index, outcome.pane_id) else {
            return rejected_with_effects("the new pane is unavailable", effects);
        };

        Handled::navigating_with_effects(
            EndpointReply::PaneInfo {
                pane: Box::new(pane),
            },
            workspace_id,
            effects,
        )
    }

    /// Focuses the pane and moves the requester onto its workspace, even when
    /// the pane already holds focus.
    pub(super) fn handle_pane_focus(&mut self, target: &PaneTarget) -> HandlerResult {
        let (ws_idx, pane_id) = self.endpoint_pane(&target.pane_id)?;
        let effects = self.state.focus_pane_in_workspace(ws_idx, pane_id).into();

        let Some(pane) = self.pane_info(ws_idx, pane_id) else {
            return Err(HandlerError {
                error: pane_missing(&target.pane_id),
                effects,
            });
        };
        Handled::navigating_with_effects(
            EndpointReply::PaneInfo {
                pane: Box::new(pane),
            },
            target.pane_id.workspace_id().clone(),
            effects,
        )
    }

    pub(super) fn handle_pane_input_set(&mut self, params: &PaneInputSetParams) -> HandlerResult {
        let (ws_idx, pane_id) = self.endpoint_pane(&params.pane_id)?;
        let right_click_passthrough = matches!(
            params.right_click,
            shepr_protocol::command::PaneRightClickTarget::Pane
        );
        let outcome = self
            .state
            .set_pane_input(ws_idx, pane_id, right_click_passthrough)
            .ok_or_else(|| pane_missing(&params.pane_id))?;
        Handled::done_with_effects(outcome.into())
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
            return Err(pane_missing(&params.pane_id).into());
        };
        let outcome = self
            .state
            .rename_terminal(&terminal_id, normalized_user_label(params.label))
            .ok_or_else(|| pane_missing(&params.pane_id))?;
        let effects = outcome.into();
        let Some(pane) = self.pane_info(ws_idx, pane_id) else {
            return Err(HandlerError {
                error: pane_missing(&params.pane_id),
                effects,
            });
        };

        Handled::reply_with_effects(
            EndpointReply::PaneInfo {
                pane: Box::new(pane),
            },
            effects,
        )
    }

    pub(super) fn handle_pane_close(&mut self, target: &PaneTarget) -> HandlerResult {
        let (ws_idx, pane_id) = self.endpoint_pane(&target.pane_id)?;
        let Some(plan) = self.state.prepare_pane_removal(ws_idx, pane_id) else {
            return Err(pane_missing(&target.pane_id).into());
        };
        let PaneRemovalCommit::Removed(outcome) = self.state.commit_pane_removal(&plan) else {
            return Err(pane_missing(&target.pane_id).into());
        };
        let effects = EndpointEffects::from(&outcome);
        self.shutdown_detached_terminal_runtimes(&outcome.detached_terminal_ids);

        Handled::done_with_effects(effects)
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

fn invalid_agent<T>() -> Result<T, ApiError> {
    failure(ApiErrorCode::InvalidAgent, "agent label must not be empty")
}

#[cfg(test)]
mod tests;
