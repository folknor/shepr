use super::*;

impl App {
    pub(crate) fn handle_pane_focus_direction(
        &mut self,
        params: &PaneFocusDirectionParams,
    ) -> shepr_api::error::ApiResult {
        // Direction and edges use the tiled layout even when this tab is zoomed,
        // matching TUI navigation. The layout snapshot signals zoom separately.
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
        let Some(source_public_id) = self.public_pane_id(ws_idx, source_pane_id) else {
            return Err(pane_not_found(params.pane_id.as_deref()));
        };
        let target =
            self.directional_pane_target(ws_idx, tab_idx, source_pane_id, params.direction);
        let reason = target
            .is_none()
            .then_some(PaneFocusDirectionReason::NoNeighbor);

        if let Some(target_pane_id) = target {
            self.state.focus_pane_in_workspace(ws_idx, target_pane_id);
            self.state.switch_workspace_tab(ws_idx, tab_idx);
            self.state.mode = crate::app::Mode::Terminal;
        }
        let focused_pane_id = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.tabs().get(tab_idx))
            .map(|tab| tab.layout().focused())
            .and_then(|pane_id| self.public_pane_id(ws_idx, pane_id));
        let Some(layout) = self.pane_layout_snapshot(ws_idx, tab_idx) else {
            return failure(
                shepr_api::error::ApiErrorCode::PaneLayoutUnavailable,
                "pane layout unavailable",
            );
        };

        success(ResponseResult::PaneFocusDirection {
            focus: PaneFocusDirectionResult {
                changed: target.is_some(),
                reason,
                source_pane_id: source_public_id,
                focused_pane_id,
                layout,
            },
        })
    }

    pub(crate) fn handle_pane_resize(
        &mut self,
        params: &PaneResizeParams,
    ) -> shepr_api::error::ApiResult {
        // Direction and edges use the tiled layout even when this tab is zoomed,
        // matching TUI navigation. The layout snapshot signals zoom separately.
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
        let Some(pane_public_id) = self.public_pane_id(ws_idx, pane_id) else {
            return Err(pane_not_found(params.pane_id.as_deref()));
        };

        let amount = params
            .amount
            .filter(|amount| amount.is_finite())
            .unwrap_or(crate::limits::DEFAULT_PANE_RESIZE_AMOUNT)
            .abs()
            .min(crate::limits::MAX_PANE_RESIZE_AMOUNT);
        let direction: NavDirection = super::nav_direction(params.direction);
        let area = shepr_mux::workspace::layout_rect(self.state.view.terminal_area);
        let changed = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .is_some_and(|ws| ws.resize_pane_in_tab(tab_idx, pane_id, direction, amount, area));
        if changed {
            self.schedule_session_save();
        }

        let Some(layout) = self.pane_layout_snapshot(ws_idx, tab_idx) else {
            return failure(
                shepr_api::error::ApiErrorCode::PaneLayoutUnavailable,
                "pane layout unavailable",
            );
        };
        let focused_pane_id = layout.focused_pane_id.clone();

        success(ResponseResult::PaneResize {
            resize: PaneResizeResult {
                changed,
                reason: (!changed).then_some(PaneResizeReason::Unchanged),
                pane_id: pane_public_id,
                focused_pane_id,
                layout,
            },
        })
    }

    pub(crate) fn handle_pane_swap(
        &mut self,
        params: PaneSwapParams,
    ) -> shepr_api::error::ApiResult {
        // Direction and edges use the tiled layout even when this tab is zoomed,
        // matching TUI navigation. The layout snapshot signals zoom separately.
        let directional = params.direction.is_some();
        let explicit = params.source_pane_id.is_some() || params.target_pane_id.is_some();
        if directional == explicit {
            return failure(
                shepr_api::error::ApiErrorCode::InvalidPaneSwap,
                "provide either direction with optional pane_id, or source_pane_id and target_pane_id",
            );
        }

        let (ws_idx, tab_idx, source_pane_id, target_pane_id, reason) = if let Some(direction) =
            params.direction
        {
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
            let target = self.directional_pane_target(ws_idx, tab_idx, source_pane_id, direction);
            match target {
                Some(target_pane_id) => (
                    ws_idx,
                    tab_idx,
                    Some(source_pane_id),
                    Some(target_pane_id),
                    None,
                ),
                None => (
                    ws_idx,
                    tab_idx,
                    Some(source_pane_id),
                    None,
                    Some(PaneSwapReason::NoNeighbor),
                ),
            }
        } else {
            let Some(source_raw) = params.source_pane_id.as_deref() else {
                return failure(
                    shepr_api::error::ApiErrorCode::InvalidPaneSwap,
                    "missing source_pane_id",
                );
            };
            let Some(target_raw) = params.target_pane_id.as_deref() else {
                return failure(
                    shepr_api::error::ApiErrorCode::InvalidPaneSwap,
                    "missing target_pane_id",
                );
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
            let response_context = source
                .map(|(ws_idx, tab_idx, _)| (ws_idx, tab_idx))
                .or_else(|| target.map(|(ws_idx, tab_idx, _)| (ws_idx, tab_idx)))
                .or_else(|| {
                    let ws_idx = self.state.active_index()?;
                    let tab_idx = self.state.workspaces.get(ws_idx)?.active_tab_index();
                    Some((ws_idx, tab_idx))
                });
            let Some((ws_idx, tab_idx)) = response_context else {
                return failure(
                    shepr_api::error::ApiErrorCode::PaneLayoutUnavailable,
                    "pane layout unavailable",
                );
            };
            // An unresolved source leaves nothing to swap (the reason below
            // is `NotFound`), and its public id is echoed from the request.
            let source_pane_id = source.map(|(_, _, pane_id)| pane_id);
            let target_pane_id = target.map(|(_, _, pane_id)| pane_id);
            let reason = match (source, target) {
                (None, _) | (_, None) => Some(PaneSwapReason::NotFound),
                (Some((_, _, source)), Some((_, _, target))) if source == target => {
                    Some(PaneSwapReason::SamePane)
                }
                (Some((source_ws, source_tab, _)), Some((target_ws, target_tab, _)))
                    if source_ws != target_ws || source_tab != target_tab =>
                {
                    Some(PaneSwapReason::CrossTab)
                }
                _ => None,
            };
            (ws_idx, tab_idx, source_pane_id, target_pane_id, reason)
        };

        let mut changed = false;
        if reason.is_none()
            && let Some(source_pane_id) = source_pane_id
            && let Some(target_pane_id) = target_pane_id
        {
            let previous_focus = self.state.current_pane_focus_target();
            if let Some(workspace) = self.state.workspaces.get_mut(ws_idx) {
                changed = workspace.swap_panes_in_tab(tab_idx, source_pane_id, target_pane_id);
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

        // A swap answers with the caller's own text for an id that does not
        // resolve, so both ids stay strings.
        let source_public_id = match params.source_pane_id {
            Some(raw) => self
                .parse_pane_id(&raw)
                .and_then(|(idx, pane_id)| {
                    self.state
                        .workspaces
                        .get(idx)?
                        .find_tab_index_for_pane(pane_id)?;
                    self.public_pane_id(idx, pane_id)
                })
                .map_or(raw, |id| id.to_string()),
            None => source_pane_id
                .and_then(|pane_id| self.public_pane_id(ws_idx, pane_id))
                .map_or_default(|id| id.to_string()),
        };
        let target_public_id = match params.target_pane_id {
            Some(raw) => Some(
                self.parse_pane_id(&raw)
                    .and_then(|(idx, pane_id)| {
                        self.state
                            .workspaces
                            .get(idx)?
                            .find_tab_index_for_pane(pane_id)?;
                        self.public_pane_id(idx, pane_id)
                    })
                    .map_or(raw, |id| id.to_string()),
            ),
            None => target_pane_id
                .and_then(|pane_id| self.public_pane_id(ws_idx, pane_id))
                .map(|id| id.to_string()),
        };
        let Some(layout) = self.pane_layout_snapshot(ws_idx, tab_idx) else {
            return failure(
                shepr_api::error::ApiErrorCode::PaneLayoutUnavailable,
                "pane layout unavailable",
            );
        };
        let focused_pane_id = layout.focused_pane_id.clone();

        success(ResponseResult::PaneSwap {
            swap: PaneSwapResult {
                changed,
                reason,
                source_pane_id: source_public_id,
                target_pane_id: target_public_id,
                focused_pane_id,
                layout,
            },
        })
    }

    pub(crate) fn handle_pane_zoom(
        &mut self,
        params: &PaneZoomParams,
    ) -> shepr_api::error::ApiResult {
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
        let Some(pane_public_id) = self.public_pane_id(ws_idx, pane_id) else {
            return Err(pane_not_found(params.pane_id.as_deref()));
        };
        let command = match params.mode {
            PaneZoomMode::Toggle => PaneZoomCommand::Toggle,
            PaneZoomMode::On => PaneZoomCommand::On,
            PaneZoomMode::Off => PaneZoomCommand::Off,
        };
        let Some(outcome) = self.state.apply_pane_zoom(ws_idx, pane_id, command) else {
            return Err(pane_not_found(Some(&pane_public_id)));
        };
        if outcome.changed || outcome.focus_changed {
            self.schedule_session_save();
        }
        self.state.mode = crate::app::Mode::Terminal;
        let Some(layout) = self.pane_layout_snapshot(ws_idx, tab_idx) else {
            return failure(
                shepr_api::error::ApiErrorCode::PaneLayoutUnavailable,
                "pane layout unavailable",
            );
        };
        let focused_pane_id = layout.focused_pane_id.clone();

        success(ResponseResult::PaneZoom {
            zoom: PaneZoomResult {
                changed: outcome.changed || outcome.focus_changed,
                zoom_changed: outcome.changed,
                focus_changed: outcome.focus_changed,
                reason: outcome.reason.map(|reason| match reason {
                    PaneZoomNoopReason::SinglePane => PaneZoomReason::SinglePane,
                    PaneZoomNoopReason::AlreadyZoomed => PaneZoomReason::AlreadyZoomed,
                    PaneZoomNoopReason::AlreadyUnzoomed => PaneZoomReason::AlreadyUnzoomed,
                }),
                pane_id: pane_public_id,
                focused_pane_id,
                zoomed: outcome.zoomed,
                layout,
            },
        })
    }
}
