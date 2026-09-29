use super::*;

impl App {
    pub(crate) fn handle_pane_focus_direction(
        &mut self,
        params: &PaneFocusDirectionParams,
    ) -> EndpointResult {
        // Direction and edges use the tiled layout even when this tab is
        // zoomed, matching TUI navigation.
        let Some((ws_idx, source_pane_id)) = self.resolve_optional_pane(params.pane_id.as_deref())
        else {
            return Err(pane_not_found(params.pane_id.as_deref()));
        };
        let Some(tab_idx) = self.tab_index_for_pane(ws_idx, source_pane_id) else {
            return Err(pane_not_found(
                self.public_pane_id(ws_idx, source_pane_id)
                    .as_deref()
                    .or(params.pane_id.as_deref()),
            ));
        };
        if self.public_pane_id(ws_idx, source_pane_id).is_none() {
            return Err(pane_not_found(params.pane_id.as_deref()));
        }
        // No neighbour in that direction is a successful no-op.
        if let Some(target_pane_id) =
            self.directional_pane_target(ws_idx, tab_idx, source_pane_id, params.direction)
        {
            self.state.focus_pane_in_workspace(ws_idx, target_pane_id);
            self.state.switch_workspace_tab(ws_idx, tab_idx);
            self.state.mode = crate::app::Mode::Terminal;
        }
        Ok(EndpointReply::Done)
    }

    pub(crate) fn handle_pane_resize(&mut self, params: &PaneResizeParams) -> EndpointResult {
        // Direction and edges use the tiled layout even when this tab is
        // zoomed, matching TUI navigation.
        let Some((ws_idx, pane_id)) = self.resolve_optional_pane(params.pane_id.as_deref()) else {
            return Err(pane_not_found(params.pane_id.as_deref()));
        };
        let Some(tab_idx) = self.tab_index_for_pane(ws_idx, pane_id) else {
            return Err(pane_not_found(
                self.public_pane_id(ws_idx, pane_id)
                    .as_deref()
                    .or(params.pane_id.as_deref()),
            ));
        };
        if self.public_pane_id(ws_idx, pane_id).is_none() {
            return Err(pane_not_found(params.pane_id.as_deref()));
        }

        let amount = params
            .amount
            .filter(|amount| amount.is_finite())
            .unwrap_or(crate::limits::DEFAULT_PANE_RESIZE_AMOUNT)
            .abs()
            .min(crate::limits::MAX_PANE_RESIZE_AMOUNT);
        let direction: NavDirection = super::nav_direction(params.direction);
        let area = shepr_mux::workspace::layout_rect(self.state.tab_layout_area(ws_idx, tab_idx));
        // A resize that moves no split edge is a successful no-op.
        let changed = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .is_some_and(|ws| ws.resize_pane_in_tab(tab_idx, pane_id, direction, amount, area));
        if changed {
            self.schedule_session_save();
        }
        Ok(EndpointReply::Done)
    }

    /// Swaps two panes of one tab, named by a direction from `pane_id` (the
    /// focused pane when absent) or by an explicit source and target. A swap
    /// with nothing to swap (no neighbour, an unknown pane, the same pane
    /// twice, or panes in different tabs) is a successful no-op.
    pub(crate) fn handle_pane_swap(&mut self, params: &PaneSwapParams) -> EndpointResult {
        let directional = params.direction.is_some();
        let explicit = params.source_pane_id.is_some() || params.target_pane_id.is_some();
        if directional == explicit {
            return failure(
                ApiErrorCode::InvalidPaneSwap,
                "provide either direction with optional pane_id, or source_pane_id and target_pane_id",
            );
        }

        let swap = if let Some(direction) = params.direction {
            let Some((ws_idx, source_pane_id)) =
                self.resolve_swap_source(params.pane_id.as_deref())
            else {
                return Err(pane_not_found(params.pane_id.as_deref()));
            };
            let Some(tab_idx) = self.tab_index_for_pane(ws_idx, source_pane_id) else {
                return Err(pane_not_found(
                    self.public_pane_id(ws_idx, source_pane_id)
                        .as_deref()
                        .or(params.pane_id.as_deref()),
                ));
            };
            self.directional_pane_target(ws_idx, tab_idx, source_pane_id, direction)
                .map(|target_pane_id| (ws_idx, tab_idx, source_pane_id, target_pane_id))
        } else {
            let Some(source_raw) = params.source_pane_id.as_deref() else {
                return failure(ApiErrorCode::InvalidPaneSwap, "missing source_pane_id");
            };
            let Some(target_raw) = params.target_pane_id.as_deref() else {
                return failure(ApiErrorCode::InvalidPaneSwap, "missing target_pane_id");
            };
            let source = self
                .parse_pane_id(source_raw)
                .and_then(|(ws_idx, pane_id)| {
                    let tab_idx = self.tab_index_for_pane(ws_idx, pane_id)?;
                    Some((ws_idx, tab_idx, pane_id))
                });
            let target = self
                .parse_pane_id(target_raw)
                .and_then(|(ws_idx, pane_id)| {
                    let tab_idx = self.tab_index_for_pane(ws_idx, pane_id)?;
                    Some((ws_idx, tab_idx, pane_id))
                });
            if source.is_none() && target.is_none() && self.state.active_index().is_none() {
                return failure(
                    ApiErrorCode::PaneLayoutUnavailable,
                    "pane layout unavailable",
                );
            }
            match (source, target) {
                (Some((source_ws, source_tab, source)), Some((target_ws, target_tab, target)))
                    if source != target && source_ws == target_ws && source_tab == target_tab =>
                {
                    Some((source_ws, source_tab, source, target))
                }
                _ => None,
            }
        };

        if let Some((ws_idx, tab_idx, source_pane_id, target_pane_id)) = swap {
            let previous_focus = self.state.current_pane_focus_target();
            if let Some(workspace) = self.state.workspaces.get_mut(ws_idx) {
                let changed = workspace.swap_panes_in_tab(tab_idx, source_pane_id, target_pane_id);
                workspace.focus_pane_in_tab(tab_idx, source_pane_id);
                if changed {
                    self.state.switch_workspace_tab(ws_idx, tab_idx);
                    self.state
                        .record_pane_focus_change(previous_focus, ws_idx, source_pane_id);
                    self.state.mark_session_dirty();
                    self.schedule_session_save();
                }
            }
        }
        Ok(EndpointReply::Done)
    }

    pub(crate) fn handle_pane_zoom(&mut self, params: &PaneZoomParams) -> EndpointResult {
        let Some((ws_idx, pane_id)) = self.resolve_optional_pane(params.pane_id.as_deref()) else {
            return Err(pane_not_found(params.pane_id.as_deref()));
        };
        if self.tab_index_for_pane(ws_idx, pane_id).is_none() {
            return Err(pane_not_found(
                self.public_pane_id(ws_idx, pane_id)
                    .as_deref()
                    .or(params.pane_id.as_deref()),
            ));
        }
        let Some(pane_public_id) = self.public_pane_id(ws_idx, pane_id) else {
            return Err(pane_not_found(params.pane_id.as_deref()));
        };
        let command = match params.mode {
            PaneZoomMode::Toggle => PaneZoomCommand::Toggle,
            PaneZoomMode::On => PaneZoomCommand::On,
            PaneZoomMode::Off => PaneZoomCommand::Off,
        };
        // A zoom that is already in the asked state is a successful no-op.
        let Some(outcome) = self.state.apply_pane_zoom(ws_idx, pane_id, command) else {
            return Err(pane_not_found(Some(&pane_public_id)));
        };
        if outcome.changed || outcome.focus_changed {
            self.schedule_session_save();
        }
        self.state.mode = crate::app::Mode::Terminal;
        Ok(EndpointReply::Done)
    }
}
