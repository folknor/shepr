use super::*;

impl App {
    /// Focuses the neighbour of the pane in the given direction and moves the
    /// requester onto its workspace. A neighbour that already holds focus
    /// still counts as found; at an edge nothing changes and nobody moves.
    pub(crate) fn handle_pane_focus_direction(
        &mut self,
        params: &PaneFocusDirectionParams,
    ) -> HandlerResult {
        // Direction and edges use the tiled layout even when this workspace is
        // zoomed, matching TUI navigation.
        let (ws_idx, source_pane_id) = self.endpoint_pane(&params.pane_id)?;
        let Some(target_pane_id) =
            self.directional_pane_target(ws_idx, source_pane_id, params.direction)
        else {
            return Handled::done();
        };
        let effects = self
            .state
            .focus_pane_in_workspace(ws_idx, target_pane_id)
            .into();
        Handled::navigating_with_effects(
            EndpointReply::Done,
            params.pane_id.workspace_id().clone(),
            effects,
        )
    }

    pub(crate) fn handle_pane_resize(&mut self, params: &PaneResizeParams) -> HandlerResult {
        // Direction and edges use the tiled layout even when this workspace is
        // zoomed, matching TUI navigation.
        let (ws_idx, pane_id) = self.endpoint_pane(&params.pane_id)?;
        let direction: NavDirection = super::nav_direction(params.direction);
        let area = shepr_mux::workspace::layout_rect(self.state.workspace_layout_area(ws_idx));
        // A resize that moves no split edge is a successful no-op.
        let outcome = self.state.edit_workspace_geometry(ws_idx, |workspace| {
            workspace.resize_pane(
                pane_id,
                direction,
                crate::limits::DEFAULT_PANE_RESIZE_AMOUNT,
                area,
            )
        });
        Handled::done_with_effects(outcome.into())
    }

    /// Swaps two panes of one workspace, named by a direction from a pane or by
    /// an explicit source and target. The requester follows the swap to its
    /// workspace only when the layout changed. An unknown pane named by a
    /// direction is refused; a swap with nothing to swap (no neighbour, stale
    /// or identical ids, panes of different workspaces) is a successful no-op.
    pub(crate) fn handle_pane_swap(&mut self, params: &PaneSwapParams) -> HandlerResult {
        let swap = match params {
            PaneSwapParams::Direction { pane_id, direction } => {
                let (ws_idx, source_pane_id) = self.endpoint_pane(pane_id)?;
                self.directional_pane_target(ws_idx, source_pane_id, *direction)
                    .map(|target_pane_id| (ws_idx, source_pane_id, target_pane_id))
            }
            PaneSwapParams::Panes { source, target } => {
                match (self.resolve_pane_id(source), self.resolve_pane_id(target)) {
                    (Some((source_ws, source)), Some((target_ws, target)))
                        if source != target && source_ws == target_ws =>
                    {
                        Some((source_ws, source, target))
                    }
                    _ => None,
                }
            }
        };

        let Some((ws_idx, source_pane_id, target_pane_id)) = swap else {
            return Handled::done();
        };
        let Some(workspace_id) = self.public_workspace_id(ws_idx) else {
            return Handled::done();
        };
        let outcome = self
            .state
            .swap_workspace_panes(ws_idx, source_pane_id, target_pane_id);
        if !outcome.changed() {
            return Handled::done();
        }
        Handled::navigating_with_effects(EndpointReply::Done, workspace_id, outcome.into())
    }

    /// Toggles the zoom of the pane's workspace, focusing the pane first, and
    /// moves the requester onto that workspace even when the toggle changes
    /// nothing (a workspace of one pane has nothing to zoom over).
    pub(crate) fn handle_pane_zoom(&mut self, params: &PaneZoomParams) -> HandlerResult {
        let (ws_idx, pane_id) = self.endpoint_pane(&params.pane_id)?;
        let Some(outcome) = self.state.toggle_pane_zoom(ws_idx, pane_id) else {
            // toggle_pane_zoom returns None only when the pane is absent. Its
            // one-pane zoom no-op is handled before set_zoomed can refuse it.
            return Err(pane_missing(&params.pane_id).into());
        };
        Handled::navigating_with_effects(
            EndpointReply::Done,
            params.pane_id.workspace_id().clone(),
            outcome.into(),
        )
    }
}
