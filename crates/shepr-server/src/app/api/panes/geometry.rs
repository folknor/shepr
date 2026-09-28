use super::*;

impl App {
    pub(crate) fn handle_pane_layout(
        &mut self,
        params: &PaneLayoutParams,
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
        let Some(layout) = self.pane_layout_snapshot(ws_idx, tab_idx) else {
            return failure(
                shepr_api::error::ApiErrorCode::PaneLayoutUnavailable,
                "pane layout unavailable",
            );
        };

        success(ResponseResult::PaneLayout { layout })
    }

    pub(crate) fn handle_pane_process_info(
        &mut self,
        params: &PaneProcessInfoParams,
    ) -> shepr_api::error::ApiResult {
        let Some((ws_idx, pane_id)) = self.resolve_optional_pane(params.pane_id.as_deref()) else {
            return Err(pane_not_found(params.pane_id.as_deref()));
        };
        let public_pane_id = self.public_pane_id(ws_idx, pane_id);
        let Some((runtime, _workspace_id)) = self.lookup_runtime(ws_idx, pane_id) else {
            return Err(pane_not_found(
                public_pane_id.as_deref().or(params.pane_id.as_deref()),
            ));
        };
        let Some(public_pane_id) = public_pane_id else {
            return Err(pane_not_found(params.pane_id.as_deref()));
        };
        let shell_pid = runtime.child_pid();
        let foreground_job = shell_pid.and_then(shepr_agent::detect::foreground_job);
        let foreground_process_group_id = foreground_job.as_ref().map(|job| job.process_group_id);
        let foreground_processes = foreground_job.map_or_default(|job| {
            job.processes
                .into_iter()
                .map(|process| PaneProcessInfoProcess {
                    pid: process.pid,
                    name: process.name,
                    argv0: process.argv0,
                    argv: process.argv,
                    cmdline: process.cmdline,
                    cwd: shepr_agent::detect::process_cwd(process.pid)
                        .map(|cwd| cwd.display().to_string()),
                })
                .collect()
        });

        success(ResponseResult::PaneProcessInfo {
            process_info: PaneProcessInfo {
                pane_id: public_pane_id,
                shell_pid,
                foreground_process_group_id,
                foreground_processes,
            },
        })
    }

    pub(crate) fn handle_pane_neighbor(
        &mut self,
        params: &PaneNeighborParams,
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
        let Some(source_public_id) = self.public_pane_id(ws_idx, pane_id) else {
            return Err(pane_not_found(params.pane_id.as_deref()));
        };
        let neighbor_pane_id = self
            .directional_pane_target(ws_idx, tab_idx, pane_id, params.direction)
            .and_then(|pane_id| self.public_pane_id(ws_idx, pane_id));
        let Some(layout) = self.pane_layout_snapshot(ws_idx, tab_idx) else {
            return failure(
                shepr_api::error::ApiErrorCode::PaneLayoutUnavailable,
                "pane layout unavailable",
            );
        };

        success(ResponseResult::PaneNeighbor {
            neighbor: PaneNeighborResult {
                pane_id: source_public_id,
                direction: params.direction,
                neighbor_pane_id,
                layout,
            },
        })
    }

    pub(crate) fn handle_pane_edges(
        &mut self,
        params: &PaneEdgesParams,
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
        let Some(tab) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.tabs().get(tab_idx))
        else {
            return failure(
                shepr_api::error::ApiErrorCode::PaneLayoutUnavailable,
                "pane layout unavailable",
            );
        };
        let area = self.state.view.terminal_area;
        let Some(info) = tab
            .layout
            .panes(area)
            .into_iter()
            .find(|info| info.id == pane_id)
        else {
            return Err(pane_not_found(
                self.public_pane_id(ws_idx, pane_id)
                    .as_deref()
                    .or(params.pane_id.as_deref()),
            ));
        };
        let Some(pane_public_id) = self.public_pane_id(ws_idx, pane_id) else {
            return Err(pane_not_found(params.pane_id.as_deref()));
        };
        let Some(layout) = self.pane_layout_snapshot(ws_idx, tab_idx) else {
            return failure(
                shepr_api::error::ApiErrorCode::PaneLayoutUnavailable,
                "pane layout unavailable",
            );
        };

        success(ResponseResult::PaneEdges {
            edges: PaneEdgesResult {
                pane_id: pane_public_id,
                left: info.rect.x <= area.x,
                right: info.rect.x + info.rect.width >= area.x + area.width,
                up: info.rect.y <= area.y,
                down: info.rect.y + info.rect.height >= area.y + area.height,
                layout,
            },
        })
    }

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
            .map(|tab| tab.layout.focused())
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
            .unwrap_or(0.05)
            .abs()
            .min(0.5);
        let direction: NavDirection = super::nav_direction(params.direction);
        let area = self.state.view.terminal_area;
        let changed = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .and_then(|ws| ws.tabs_mut().get_mut(tab_idx))
            .is_some_and(|tab| tab.layout.resize_pane(pane_id, direction, amount, area));
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
        if changed {
            self.emit_layout_updated_snapshot(layout.clone());
        }

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
                Some(target_pane_id) => {
                    (ws_idx, tab_idx, source_pane_id, Some(target_pane_id), None)
                }
                None => (
                    ws_idx,
                    tab_idx,
                    source_pane_id,
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
            let source_pane_id = source
                .map(|(_, _, pane_id)| pane_id)
                .or_else(|| {
                    self.state
                        .workspaces
                        .get(ws_idx)?
                        .tabs()
                        .get(tab_idx)
                        .map(|tab| tab.layout.focused())
                })
                .unwrap_or(PaneId::from_raw(0));
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
            && let Some(target_pane_id) = target_pane_id
        {
            let previous_focus = self.state.current_pane_focus_target();
            if let Some(tab) = self
                .state
                .workspaces
                .get_mut(ws_idx)
                .and_then(|ws| ws.tabs_mut().get_mut(tab_idx))
            {
                changed = tab.layout.swap_panes(source_pane_id, target_pane_id);
                tab.layout.focus_pane(source_pane_id);
                if changed {
                    self.state.switch_workspace_tab(ws_idx, tab_idx);
                    self.state
                        .record_pane_focus_change(previous_focus, ws_idx, source_pane_id);
                    self.state.mark_session_dirty();
                    self.schedule_session_save();
                }
            }
        }

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
                .unwrap_or(raw),
            None => self
                .public_pane_id(ws_idx, source_pane_id)
                .unwrap_or_default(),
        };
        let target_public_id = match params.target_pane_id {
            Some(raw) => self
                .parse_pane_id(&raw)
                .and_then(|(idx, pane_id)| {
                    self.state
                        .workspaces
                        .get(idx)?
                        .find_tab_index_for_pane(pane_id)?;
                    self.public_pane_id(idx, pane_id)
                })
                .or(Some(raw)),
            None => target_pane_id.and_then(|pane_id| self.public_pane_id(ws_idx, pane_id)),
        };
        let Some(layout) = self.pane_layout_snapshot(ws_idx, tab_idx) else {
            return failure(
                shepr_api::error::ApiErrorCode::PaneLayoutUnavailable,
                "pane layout unavailable",
            );
        };
        let focused_pane_id = layout.focused_pane_id.clone();
        if changed {
            self.emit_layout_updated_snapshot(layout.clone());
        }

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

    pub(crate) fn handle_pane_move(
        &mut self,
        params: PaneMoveParams,
    ) -> shepr_api::error::ApiResult {
        let PaneMoveParams {
            pane_id,
            destination,
            focus,
        } = params;
        let Some((source_ws_idx, source_pane_id)) = self.parse_pane_id(&pane_id) else {
            return Err(pane_not_found(Some(&pane_id)));
        };
        let Some(source_tab_idx) = self.tab_index_for_pane(source_ws_idx, source_pane_id) else {
            return Err(pane_not_found(Some(&pane_id)));
        };
        let Some((source_ws, source_tab)) = self
            .state
            .workspaces
            .get(source_ws_idx)
            .and_then(|ws| Some((ws, ws.tabs().get(source_tab_idx)?)))
        else {
            return Err(pane_not_found(Some(&pane_id)));
        };
        let Some(source_terminal_id) = source_tab.terminal_id(source_pane_id).cloned() else {
            return Err(pane_not_found(Some(&pane_id)));
        };
        let source_tab_zoomed = source_tab.zoomed;
        let previous_workspace_label = source_ws.custom_name.clone();
        let previous_tab_label = source_tab.custom_name.clone();
        let identity_cwd = source_ws.identity_cwd.clone();
        let previous_pane_id = self
            .public_pane_id(source_ws_idx, source_pane_id)
            .unwrap_or_else(|| pane_id.clone());
        let Some(previous_workspace_id) = self.public_workspace_id(source_ws_idx) else {
            return Err(pane_not_found(Some(&pane_id)));
        };
        let Some(previous_tab_id) = self.public_tab_id(source_ws_idx, source_tab_idx) else {
            return Err(tab_for_pane_not_found(&pane_id));
        };
        let recovery_context = PaneMoveRecoveryContext {
            source_ws_idx,
            previous_workspace_id: previous_workspace_id.clone(),
            previous_workspace_label,
            previous_tab_label,
            identity_cwd,
        };

        if source_tab_zoomed {
            let Some(layout) = self.pane_layout_snapshot(source_ws_idx, source_tab_idx) else {
                return failure(
                    shepr_api::error::ApiErrorCode::PaneLayoutUnavailable,
                    "pane layout unavailable",
                );
            };
            let Some(pane) = self.pane_info(source_ws_idx, source_pane_id) else {
                return Err(pane_not_found(Some(&pane_id)));
            };
            return encode_unchanged_pane_move(
                PaneMoveReason::ZoomedTab,
                previous_pane_id,
                previous_workspace_id,
                previous_tab_id,
                pane,
                Some(layout.clone()),
                layout,
            );
        }

        let resolved = match destination {
            PaneMoveDestination::Tab {
                tab_id,
                target_pane_id,
                split,
                ratio,
            } => {
                let Some((target_ws_idx, target_tab_idx)) = self.parse_tab_id(&tab_id) else {
                    return Err(tab_not_found(&tab_id));
                };
                let Some((target_tab_zoomed, target_tab_focused)) = self
                    .state
                    .workspaces
                    .get(target_ws_idx)
                    .and_then(|ws| ws.tabs().get(target_tab_idx))
                    .map(|tab| (tab.zoomed, tab.layout.focused()))
                else {
                    return Err(tab_not_found(&tab_id));
                };
                if source_ws_idx == target_ws_idx && source_tab_idx == target_tab_idx {
                    let Some(layout) = self.pane_layout_snapshot(source_ws_idx, source_tab_idx)
                    else {
                        return failure(
                            shepr_api::error::ApiErrorCode::PaneLayoutUnavailable,
                            "pane layout unavailable",
                        );
                    };
                    let Some(pane) = self.pane_info(source_ws_idx, source_pane_id) else {
                        return Err(pane_not_found(Some(&pane_id)));
                    };
                    return encode_unchanged_pane_move(
                        PaneMoveReason::SameTab,
                        previous_pane_id,
                        previous_workspace_id,
                        previous_tab_id,
                        pane,
                        Some(layout.clone()),
                        layout,
                    );
                }
                if target_tab_zoomed {
                    let Some(source_layout) =
                        self.pane_layout_snapshot(source_ws_idx, source_tab_idx)
                    else {
                        return failure(
                            shepr_api::error::ApiErrorCode::PaneLayoutUnavailable,
                            "pane layout unavailable",
                        );
                    };
                    let Some(target_layout) =
                        self.pane_layout_snapshot(target_ws_idx, target_tab_idx)
                    else {
                        return failure(
                            shepr_api::error::ApiErrorCode::PaneLayoutUnavailable,
                            "pane layout unavailable",
                        );
                    };
                    let Some(pane) = self.pane_info(source_ws_idx, source_pane_id) else {
                        return Err(pane_not_found(Some(&pane_id)));
                    };
                    return encode_unchanged_pane_move(
                        PaneMoveReason::ZoomedTab,
                        previous_pane_id,
                        previous_workspace_id,
                        previous_tab_id,
                        pane,
                        Some(source_layout),
                        target_layout,
                    );
                }
                let target_pane_id = match target_pane_id {
                    Some(raw) => {
                        let Some((pane_ws_idx, pane_id)) = self.parse_pane_id(&raw) else {
                            return Err(target_pane_not_found(&raw, None));
                        };
                        let pane_tab_idx = self.tab_index_for_pane(pane_ws_idx, pane_id);
                        if pane_ws_idx != target_ws_idx || pane_tab_idx != Some(target_tab_idx) {
                            return Err(target_pane_not_found(&raw, Some(&tab_id)));
                        }
                        pane_id
                    }
                    None => target_tab_focused,
                };
                let Some(target_tab_id) = self.public_tab_id(target_ws_idx, target_tab_idx) else {
                    return Err(tab_not_found(&tab_id));
                };
                ResolvedPaneMoveDestination::ExistingTab {
                    tab_id: target_tab_id,
                    target_pane_id,
                    split,
                    ratio: ratio.unwrap_or(0.5),
                    cross_workspace: source_ws_idx != target_ws_idx,
                }
            }
            PaneMoveDestination::NewTab {
                workspace_id,
                label,
            } => {
                let target_workspace_id = if let Some(workspace_id) = workspace_id {
                    let Some(ws_idx) = self.parse_workspace_id(&workspace_id) else {
                        return Err(workspace_not_found(&workspace_id));
                    };
                    let Some(target_workspace_id) = self.public_workspace_id(ws_idx) else {
                        return Err(workspace_not_found(&workspace_id));
                    };
                    target_workspace_id
                } else {
                    previous_workspace_id.clone()
                };
                ResolvedPaneMoveDestination::NewTab {
                    workspace_id: target_workspace_id,
                    label,
                }
            }
            PaneMoveDestination::NewWorkspace { label, tab_label } => {
                ResolvedPaneMoveDestination::NewWorkspace { label, tab_label }
            }
        };

        let previous_focus = self.state.current_pane_focus_target();
        let cross_workspace = match &resolved {
            ResolvedPaneMoveDestination::ExistingTab {
                cross_workspace, ..
            } => *cross_workspace,
            ResolvedPaneMoveDestination::NewTab { workspace_id, .. } => {
                workspace_id != &previous_workspace_id
            }
            ResolvedPaneMoveDestination::NewWorkspace { .. } => true,
        };
        let source_is_only_pane = self
            .state
            .workspaces
            .get(source_ws_idx)
            .is_some_and(|ws| ws.pane_count() == 1);

        let mut closed_workspace_id = None;
        let mut created_workspace = false;
        let mut created_tab = false;
        let source_removed_tab_id;
        let (target_ws_idx, target_tab_idx, moved_pane_id) = match resolved {
            ResolvedPaneMoveDestination::NewTab { label, .. } if !cross_workspace => {
                // One workspace call takes the pane and builds its new tab, so
                // a workspace whose only pane moves never holds an empty tab.
                let Some(new_tab) = self
                    .state
                    .workspaces
                    .get_mut(source_ws_idx)
                    .and_then(|ws| ws.move_pane_to_new_tab(source_pane_id, label))
                else {
                    return failure(
                        shepr_api::error::ApiErrorCode::PaneMoveFailed,
                        "source pane could not be moved",
                    );
                };
                source_removed_tab_id = new_tab.removed_tab_idx.map(|_| previous_tab_id.clone());
                created_tab = true;
                (source_ws_idx, new_tab.tab_idx, source_pane_id)
            }
            resolved => {
                let moved = if source_is_only_pane && cross_workspace {
                    // The source workspace goes away with its only pane: take
                    // it out of the list whole instead of emptying it in place.
                    let active_was_source = self
                        .state
                        .active
                        .as_ref()
                        .is_some_and(|id| id.as_str() == previous_workspace_id);
                    let selected_was_source = self
                        .state
                        .selected
                        .as_ref()
                        .is_some_and(|id| id.as_str() == previous_workspace_id);
                    let workspace = self.state.workspaces.remove(source_ws_idx);
                    let moved = match workspace.into_only_pane() {
                        Ok(moved) => moved,
                        Err(workspace) => {
                            self.state.workspaces.insert(source_ws_idx, *workspace);
                            return failure(
                                shepr_api::error::ApiErrorCode::PaneMoveFailed,
                                "source pane could not be moved",
                            );
                        }
                    };
                    closed_workspace_id = Some(previous_workspace_id.clone());
                    if self.state.workspaces.is_empty() {
                        self.state.set_active_index(None);
                        self.state.set_selected_index(None);
                    } else {
                        let replacement = source_ws_idx.min(self.state.workspaces.len() - 1);
                        if active_was_source {
                            self.state.set_active_index(Some(replacement));
                        }
                        if selected_was_source {
                            self.state.set_selected_index(Some(replacement));
                        }
                    }
                    source_removed_tab_id = Some(previous_tab_id.clone());
                    moved
                } else {
                    let Some(taken) = self
                        .state
                        .workspaces
                        .get_mut(source_ws_idx)
                        .and_then(|ws| ws.take_pane_for_move(source_pane_id))
                    else {
                        return failure(
                            shepr_api::error::ApiErrorCode::PaneMoveFailed,
                            "source pane could not be moved",
                        );
                    };
                    source_removed_tab_id = taken.removed_tab_idx.map(|_| previous_tab_id.clone());
                    taken.moved
                };
                if cross_workspace && let Ok(alias) = previous_pane_id.parse() {
                    self.state
                        .public_pane_id_aliases
                        .insert(alias, source_pane_id);
                }
                match resolved {
                    ResolvedPaneMoveDestination::ExistingTab {
                        tab_id,
                        target_pane_id,
                        split,
                        ratio,
                        cross_workspace: _,
                    } => {
                        let Some((target_ws_idx, target_tab_idx)) = self.parse_tab_id(&tab_id)
                        else {
                            self.recover_failed_pane_move(recovery_context, moved);
                            return failure(
                                shepr_api::error::ApiErrorCode::PaneMoveFailed,
                                "target tab disappeared",
                            );
                        };
                        let direction = split_direction_to_layout(&split);
                        let inserted = match self.state.workspaces.get_mut(target_ws_idx) {
                            Some(ws) => ws.insert_moved_pane_into_tab(
                                target_tab_idx,
                                target_pane_id,
                                moved,
                                direction,
                                ratio,
                                focus,
                            ),
                            None => Err(moved),
                        };
                        let moved_pane_id = match inserted {
                            Ok(pane_id) => pane_id,
                            Err(moved) => {
                                self.recover_failed_pane_move(recovery_context, moved);
                                return failure(
                                    shepr_api::error::ApiErrorCode::PaneMoveFailed,
                                    "target pane could not be split",
                                );
                            }
                        };
                        (target_ws_idx, target_tab_idx, moved_pane_id)
                    }
                    ResolvedPaneMoveDestination::NewTab {
                        workspace_id,
                        label,
                    } => {
                        let Some(target_ws_idx) = self.parse_workspace_id(&workspace_id) else {
                            self.recover_failed_pane_move(recovery_context, moved);
                            return failure(
                                shepr_api::error::ApiErrorCode::PaneMoveFailed,
                                "target workspace disappeared",
                            );
                        };
                        let moved_pane_id = moved.pane_id;
                        let target_tab_idx = match self.state.workspaces.get_mut(target_ws_idx) {
                            Some(ws) => ws.create_tab_from_existing_pane(moved, label),
                            None => {
                                self.recover_failed_pane_move(recovery_context, moved);
                                return failure(
                                    shepr_api::error::ApiErrorCode::PaneMoveFailed,
                                    "target workspace disappeared",
                                );
                            }
                        };
                        created_tab = true;
                        (target_ws_idx, target_tab_idx, moved_pane_id)
                    }
                    ResolvedPaneMoveDestination::NewWorkspace { label, tab_label } => {
                        let identity_cwd =
                            self.state.terminals.get(&source_terminal_id).map_or_else(
                                || {
                                    self.paths
                                        .current_dir()
                                        .unwrap_or_else(|| std::path::Path::new("/"))
                                        .to_path_buf()
                                },
                                |terminal| terminal.cwd().to_path_buf(),
                            );
                        let moved_pane_id = moved.pane_id;
                        let workspace = shepr_mux::workspace::Workspace::from_existing_pane(
                            label,
                            tab_label,
                            &identity_cwd,
                            moved,
                        );
                        self.state.workspaces.push(workspace);
                        let target_ws_idx = self.state.workspaces.len() - 1;
                        created_workspace = true;
                        created_tab = true;
                        (target_ws_idx, 0, moved_pane_id)
                    }
                }
            }
        };

        self.state.refresh_active_tab_id();
        if focus || self.state.active_index().is_none() {
            self.state
                .switch_workspace_tab(target_ws_idx, target_tab_idx);
            self.state
                .record_pane_focus_change(previous_focus, target_ws_idx, moved_pane_id);
            self.state.mode = crate::app::Mode::Terminal;
        }
        let created_workspace = if created_workspace {
            self.workspace_info(target_ws_idx)
        } else {
            None
        };
        let created_tab = if created_tab {
            self.tab_info(target_ws_idx, target_tab_idx)
        } else {
            None
        };

        self.state.mark_session_dirty();
        self.schedule_session_save();
        let Some(pane) = self.pane_info(target_ws_idx, moved_pane_id) else {
            return failure(
                shepr_api::error::ApiErrorCode::PaneMoveFailed,
                "moved pane is unavailable",
            );
        };
        let source_layout = if closed_workspace_id.is_none() {
            self.parse_tab_id(&previous_tab_id)
                .and_then(|(ws_idx, tab_idx)| self.pane_layout_snapshot(ws_idx, tab_idx))
        } else {
            None
        };
        let Some(target_layout) = self.pane_layout_snapshot(target_ws_idx, target_tab_idx) else {
            return failure(
                shepr_api::error::ApiErrorCode::PaneLayoutUnavailable,
                "pane layout unavailable",
            );
        };
        let focused_pane_id = target_layout.focused_pane_id.clone();
        let move_result = PaneMoveResult {
            changed: true,
            reason: None,
            previous_pane_id: previous_pane_id.clone(),
            previous_workspace_id: previous_workspace_id.clone(),
            previous_tab_id: previous_tab_id.clone(),
            pane: Box::new(pane.clone()),
            source_layout: source_layout.clone().map(Box::new),
            target_layout: Box::new(target_layout),
            created_workspace: created_workspace.clone(),
            created_tab: created_tab.clone(),
            closed_workspace_id: closed_workspace_id.clone(),
            closed_tab_id: source_removed_tab_id.clone(),
            focused_pane_id,
        };
        if let Some(closed_tab_id) = &source_removed_tab_id {
            self.emit_event(EventEnvelope {
                data: EventData::TabClosed {
                    tab_id: closed_tab_id.clone(),
                    workspace_id: previous_workspace_id.clone(),
                },
            });
        }
        if let Some(closed_workspace_id) = &closed_workspace_id {
            self.emit_event(EventEnvelope {
                data: EventData::WorkspaceClosed {
                    workspace_id: closed_workspace_id.clone(),
                    workspace: None,
                },
            });
        }
        if let Some(workspace) = &created_workspace {
            self.emit_event(EventEnvelope {
                data: EventData::WorkspaceCreated {
                    workspace: workspace.clone(),
                },
            });
        }
        if let Some(tab) = &created_tab {
            self.emit_event(EventEnvelope {
                data: EventData::TabCreated { tab: tab.clone() },
            });
        }
        self.emit_event(EventEnvelope {
            data: EventData::PaneMoved {
                previous_pane_id,
                previous_workspace_id,
                previous_tab_id,
                pane: Box::new(pane),
                created_workspace,
                created_tab,
                closed_workspace_id,
                closed_tab_id: source_removed_tab_id,
            },
        });
        if let Some(source_layout) = source_layout {
            self.emit_layout_updated_snapshot(source_layout);
        }
        self.emit_layout_updated_snapshot((*move_result.target_layout).clone());

        success(ResponseResult::PaneMove { move_result })
    }

    pub(super) fn recover_failed_pane_move(
        &mut self,
        context: PaneMoveRecoveryContext,
        moved: shepr_mux::workspace::MovedPane,
    ) {
        if let Some(ws) = self
            .parse_workspace_id(&context.previous_workspace_id)
            .and_then(|ws_idx| self.state.workspaces.get_mut(ws_idx))
        {
            ws.create_tab_from_existing_pane(moved, context.previous_tab_label);
        } else {
            let mut workspace = shepr_mux::workspace::Workspace::from_existing_pane(
                context.previous_workspace_label,
                context.previous_tab_label,
                &context.identity_cwd,
                moved,
            );
            workspace.id = context.previous_workspace_id.into();
            let insert_idx = context.source_ws_idx.min(self.state.workspaces.len());
            self.state.workspaces.insert(insert_idx, workspace);
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
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
        if outcome.changed || outcome.focus_changed {
            self.emit_layout_updated_snapshot(layout.clone());
        }

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
