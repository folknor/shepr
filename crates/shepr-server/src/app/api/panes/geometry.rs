use super::*;

/// The fraction of a split one resize step moves its edge by.
const DEFAULT_PANE_RESIZE_AMOUNT: shepr_core::layout::RatioDelta =
    shepr_core::layout::RatioDelta::new(0.05);

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
        let (workspace_id, source_pane_id) = self.endpoint_pane(&params.pane_id)?;
        let Some(target_pane_id) =
            self.directional_pane_target(&workspace_id, source_pane_id, params.direction)
        else {
            return Handled::done();
        };
        let effects = self.state.focus_pane(target_pane_id).into();
        Handled::navigating_with_effects(
            EndpointReply::Done,
            *params.pane_id.workspace_id(),
            effects,
        )
    }

    pub(crate) fn handle_pane_resize(&mut self, params: &PaneResizeParams) -> HandlerResult {
        // Direction and edges use the tiled layout even when this workspace is
        // zoomed, matching TUI navigation.
        let (workspace_id, pane_id) = self.endpoint_pane(&params.pane_id)?;
        let direction = params.direction;
        let area = self.state.workspace(&workspace_id).map_or_else(
            || self.state.settings().headless_rect(),
            |workspace| self.state.layout_area(workspace),
        );
        // A resize that moves no split edge is a successful no-op.
        let outcome = self
            .state
            .edit_workspace_geometry(&workspace_id, |workspace| {
                workspace.resize_pane(pane_id, direction, DEFAULT_PANE_RESIZE_AMOUNT, area)
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
                let (workspace_id, source_pane_id) = self.endpoint_pane(pane_id)?;
                self.directional_pane_target(&workspace_id, source_pane_id, *direction)
                    .map(|target_pane_id| (workspace_id, source_pane_id, target_pane_id))
            }
            PaneSwapParams::Panes { source, target } => {
                match (
                    self.state.resolve_pane(source),
                    self.state.resolve_pane(target),
                ) {
                    (Some(source), Some(target))
                        if source.id() != target.id()
                            && source.workspace().id() == target.workspace().id() =>
                    {
                        Some((source.workspace().id(), source.id(), target.id()))
                    }
                    _ => None,
                }
            }
        };

        let Some((workspace_id, source_pane_id, target_pane_id)) = swap else {
            return Handled::done();
        };
        let outcome = self.state.swap_panes(source_pane_id, target_pane_id);
        if !outcome.changed() {
            return Handled::done();
        }
        Handled::navigating_with_effects(EndpointReply::Done, workspace_id, outcome.into())
    }

    /// Toggles the zoom of the pane's workspace, focusing the pane first, and
    /// moves the requester onto that workspace even when the toggle changes
    /// nothing (a workspace of one pane has nothing to zoom over).
    pub(crate) fn handle_pane_zoom(&mut self, params: &PaneZoomParams) -> HandlerResult {
        let (_, pane_id) = self.endpoint_pane(&params.pane_id)?;
        let Some(outcome) = self.state.toggle_pane_zoom(pane_id) else {
            // toggle_pane_zoom returns None only when the pane is absent. Its
            // one-pane zoom no-op is handled before set_zoomed can refuse it.
            return Err(pane_missing(&params.pane_id).into());
        };
        Handled::navigating_with_effects(
            EndpointReply::Done,
            *params.pane_id.workspace_id(),
            outcome.into(),
        )
    }
}
