use shepr_api::error::{ApiError, ApiErrorCode};

use crate::app::{App, EndpointContext};
use shepr_api::schema::{PaneReportAgentParams, PaneReportAgentSessionParams, ResponseResult};
use shepr_core::layout::{PaneId, find_in_direction};
use shepr_protocol::command::{
    EndpointReply, PaneCopyMotionParams, PaneCopySearchParams, PaneDirection,
    PaneFocusDirectionParams, PaneInputSetParams, PaneRenameParams, PaneResizeParams,
    PaneScrollParams, PaneSelectionReadParams, PaneSplitParams, PaneSwapParams, PaneTarget,
    PaneTextRange, PaneZoomParams,
};

use super::super::api_helpers::{detect_state_from_api, normalized_user_label};
use super::endpoint::{
    EndpointEffects, Handled, HandlerError, HandlerResult, internal_with_effects, pane_missing,
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
        let (workspace_id, target_pane_id) = self.endpoint_pane(&params.pane_id)?;
        let Some(ws) = self.state.workspace(&workspace_id) else {
            return Err(pane_missing(&params.pane_id).into());
        };
        let geometry = ws
            .spawn_geometry()
            .or(ctx.requester_geometry)
            .unwrap_or_else(|| self.headless_spawn_geometry());
        let chrome = self.state.chrome_in(geometry.area);
        let follow_cwd = self.launch_cwd_for_pane(target_pane_id);
        let split_cwd = self.resolve_new_terminal_cwd(follow_cwd);
        let direction = match params.direction {
            shepr_protocol::command::SplitDirection::Right => {
                shepr_core::layout::Direction::Horizontal
            }
            shepr_protocol::command::SplitDirection::Down => {
                shepr_core::layout::Direction::Vertical
            }
        };
        let prepared = match ws.prepare_split(
            target_pane_id,
            direction,
            &chrome,
            geometry.cell_px(),
            split_cwd,
        ) {
            Ok(prepared) => prepared,
            Err(shepr_mux::workspace::SplitPreparationRefused::TargetGone) => {
                return Err(pane_missing(&params.pane_id).into());
            }
            Err(shepr_mux::workspace::SplitPreparationRefused::NumberExhausted) => {
                return Err(
                    shepr_protocol::command::EndpointError::ResourceFailure(format!(
                        "the pane number space is exhausted in workspace {workspace_id}"
                    ))
                    .into(),
                );
            }
            Err(shepr_mux::workspace::SplitPreparationRefused::LayoutRefused) => {
                return Err(shepr_protocol::command::EndpointError::ResourceFailure(
                    "the workspace layout could not accept the split".to_owned(),
                )
                .into());
            }
        };
        let runtime = match self.launch_pane(
            prepared.pane_id(),
            prepared.public_id(),
            prepared.geometry(),
            prepared.cwd(),
            shepr_mux::pane::LaunchKind::Fresh,
        ) {
            Ok(runtime) => runtime,
            Err(err) => {
                return Err(
                    shepr_protocol::command::EndpointError::ResourceFailure(format!(
                        "the pane could not be split: {err}"
                    ))
                    .into(),
                );
            }
        };
        let Some(outcome) = self.state.commit_pane_split(prepared) else {
            drop(runtime);
            return Err(pane_missing(&params.pane_id).into());
        };
        self.install_runtime(outcome.pane_id, runtime);
        let effects = EndpointEffects::from(&outcome);
        let Some(pane) = self.pane_info(outcome.pane_id) else {
            return internal_with_effects("the new pane is unavailable", effects);
        };

        Handled::navigating_with_effects(
            EndpointReply::PaneInfo {
                pane: Box::new(pane),
            },
            outcome.workspace_id,
            effects,
        )
    }

    /// Focuses the pane and moves the requester onto its workspace, even when
    /// the pane already holds focus.
    pub(super) fn handle_pane_focus(&mut self, target: &PaneTarget) -> HandlerResult {
        let (_, pane_id) = self.endpoint_pane(&target.pane_id)?;
        let effects = self.state.focus_pane(pane_id).into();

        let Some(pane) = self.pane_info(pane_id) else {
            return Err(HandlerError {
                error: pane_missing(&target.pane_id),
                effects,
            });
        };
        Handled::navigating_with_effects(
            EndpointReply::PaneInfo {
                pane: Box::new(pane),
            },
            *target.pane_id.workspace_id(),
            effects,
        )
    }

    pub(super) fn handle_pane_input_set(&mut self, params: &PaneInputSetParams) -> HandlerResult {
        let (_, pane_id) = self.endpoint_pane(&params.pane_id)?;
        let right_click_passthrough = matches!(
            params.right_click,
            shepr_protocol::command::PaneRightClickTarget::Pane
        );
        let outcome = self
            .state
            .set_pane_input(pane_id, right_click_passthrough)
            .ok_or_else(|| pane_missing(&params.pane_id))?;
        Handled::done_with_effects(outcome.into())
    }

    pub(super) fn handle_pane_rename(&mut self, params: PaneRenameParams) -> HandlerResult {
        let (_, pane_id) = self.endpoint_pane(&params.pane_id)?;
        let outcome = self
            .state
            .rename_pane(pane_id, normalized_user_label(params.label))
            .ok_or_else(|| pane_missing(&params.pane_id))?;
        let effects = outcome.into();
        let Some(pane) = self.pane_info(pane_id) else {
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
        let (_, pane_id) = self.endpoint_pane(&target.pane_id)?;
        let Some(outcome) = self.state.remove_pane(pane_id) else {
            return Err(pane_missing(&target.pane_id).into());
        };
        let effects = EndpointEffects::from(&outcome);
        self.shutdown_detached_pane_runtimes(&outcome.removed);

        Handled::done_with_effects(effects)
    }
}

impl App {
    fn directional_pane_target(
        &self,
        workspace_id: &shepr_protocol::WorkspaceId,
        source_pane_id: PaneId,
        direction: PaneDirection,
    ) -> Option<PaneId> {
        let workspace = self.state.workspace(workspace_id)?;
        let panes = workspace
            .tree()
            .layout()
            .panes(self.state.layout_area(workspace));
        let source = panes.iter().find(|pane| pane.id == source_pane_id)?;
        find_in_direction(source, direction, &panes)
    }
}

fn invalid_agent<T>() -> Result<T, ApiError> {
    failure(ApiErrorCode::InvalidAgent, "agent label must not be empty")
}

#[cfg(test)]
mod tests;
