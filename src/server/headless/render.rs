use super::*;
use crate::server::ClientId;
use crate::server::clients::RenderTargetMode;

/// The session snapshot a shell projection is built from. The projection never
/// reads the pane layout trees, so they are dropped before the snapshot is
/// copied for each shell client.
fn shell_session_snapshot(app: &app::App) -> crate::api::schema::SessionSnapshot {
    let mut snapshot = app.session_snapshot();
    snapshot.layouts = Vec::new();
    snapshot
}

impl HeadlessServer {
    fn shell_focused_runtime(
        &self,
        client_id: ClientId,
    ) -> Option<(&crate::terminal::TerminalRuntime, crate::layout::PaneId)> {
        let target = self.shell_target_for_client(client_id)?;
        let (workspace_index, tab_index) = target.resolve(&self.app.state)?;
        let tab = self
            .app
            .state
            .workspaces
            .get(workspace_index)?
            .tabs
            .get(tab_index)?;
        let pane_id = tab.layout.focused();
        self.app
            .state
            .runtime_for_pane_in_workspace(&self.app.terminal_runtimes, workspace_index, pane_id)
            .map(|runtime| (runtime, pane_id))
    }

    pub(super) fn stream_host_mouse_capture_mode(&mut self) {
        let requested = self
            .clients
            .iter()
            .filter_map(|(&client_id, client)| match &client.mode {
                ClientConnectionMode::ClientShell(shell) => {
                    let focused = shell
                        .surface_active
                        .then(|| self.shell_focused_runtime(client_id))
                        .flatten();
                    let child_requests_mouse =
                        focused.is_some_and(|(runtime, _)| runtime.mouse_reporting_enabled());
                    let sgr_pixels = client.pixel_mouse
                        && focused.is_some_and(|(runtime, _)| runtime.sgr_pixel_mouse_enabled());
                    Some((
                        client_id,
                        shell.surface_active && (shell.mouse_capture || child_requests_mouse),
                        shell.surface_active && sgr_pixels,
                    ))
                }
                ClientConnectionMode::TerminalAttach { terminal_id, .. } => {
                    let runtime = self.app.terminal_runtimes.get(terminal_id);
                    let child_requests_mouse = runtime
                        .is_some_and(crate::terminal::TerminalRuntime::mouse_reporting_enabled);
                    let sgr_pixels = child_requests_mouse
                        && client.pixel_mouse
                        && runtime
                            .is_some_and(crate::terminal::TerminalRuntime::sgr_pixel_mouse_enabled);
                    Some((client_id, child_requests_mouse, sgr_pixels))
                }
                ClientConnectionMode::TerminalPending => None,
            })
            .collect::<Vec<_>>();

        let mut broken_clients = Vec::new();
        for (client_id, enabled, sgr_pixels) in requested {
            let Some(client) = self.clients.get_mut(&client_id) else {
                continue;
            };
            if client.host_mouse_capture_active == Some(enabled)
                && client.host_sgr_pixels_active == Some(sgr_pixels)
            {
                continue;
            }
            let Some(writer) = &client.writer else {
                continue;
            };
            let serialized = match Self::frame_server_message(&ServerMessage::MouseCapture {
                enabled,
                sgr_pixels,
            }) {
                Ok(framed) => framed,
                Err(err) => {
                    warn!(err = %err, "failed to serialize mouse capture mode for client");
                    continue;
                }
            };
            if writer.control.send(serialized).is_err() {
                debug!(
                    ?client_id,
                    "client writer channel closed during mouse capture update"
                );
                broken_clients.push(client_id);
                continue;
            }
            client.host_mouse_capture_active = Some(enabled);
            client.host_sgr_pixels_active = Some(sgr_pixels);
        }

        for client_id in broken_clients {
            self.remove_client_and_resize_if_needed(client_id);
        }
    }

    pub(super) fn stream_direct_terminal_keyboard_mode(&mut self) {
        let shell_modes = self
            .clients
            .iter()
            .filter(|(_, client)| client.is_shell_client())
            .map(|(&client_id, client)| {
                let report_all = client.is_active_shell_client()
                    && self
                        .shell_focused_runtime(client_id)
                        .is_some_and(|(runtime, _)| {
                            let protocol = runtime.keyboard_protocol();
                            protocol.reports_all_keys()
                                || (protocol.reports_event_types()
                                    && runtime.modify_other_keys_level() > 0)
                        });
                (client_id, report_all)
            })
            .collect::<Vec<_>>();
        let mut broken_clients = Vec::new();
        for (client_id, report_all) in shell_modes {
            let Some(client) = self.clients.get_mut(&client_id) else {
                continue;
            };
            if client
                .shell_state()
                .is_some_and(|shell| shell.host_keyboard_report_all_active == Some(report_all))
            {
                continue;
            }
            let Some(writer) = &client.writer else {
                continue;
            };
            let serialized = match Self::frame_server_message(
                &ServerMessage::ClientShellKeyboardReportAll {
                    enabled: report_all,
                },
            ) {
                Ok(serialized) => serialized,
                Err(err) => {
                    warn!(err = %err, "failed to serialize client shell keyboard report-all mode");
                    continue;
                }
            };
            if writer.control.send(serialized).is_err() {
                broken_clients.push(client_id);
                continue;
            }
            if let Some(shell) = client.shell_state_mut() {
                shell.host_keyboard_report_all_active = Some(report_all);
            }
        }

        let requested = self
            .clients
            .iter()
            .filter_map(|(&client_id, client)| {
                let ClientConnectionMode::TerminalAttach { terminal_id, .. } = &client.mode else {
                    return None;
                };
                let (flags, modify_other_keys_level) = self
                    .app
                    .terminal_runtimes
                    .get(terminal_id)
                    .map_or((0, 0), |runtime| {
                        let flags = match runtime.keyboard_protocol() {
                            crate::input::KeyboardProtocol::Legacy => 0,
                            crate::input::KeyboardProtocol::Kitty { flags } => flags,
                        };
                        (flags, runtime.modify_other_keys_level())
                    });
                Some((client_id, flags, modify_other_keys_level))
            })
            .collect::<Vec<_>>();

        for (client_id, flags, modify_other_keys_level) in requested {
            let Some(client) = self.clients.get_mut(&client_id) else {
                continue;
            };
            if client.terminal_attach_state().is_some_and(|state| {
                state.host_keyboard_protocol_active == Some((flags, modify_other_keys_level))
            }) {
                continue;
            }
            let Some(writer) = &client.writer else {
                continue;
            };
            let serialized =
                match Self::frame_server_message(&ServerMessage::DirectTerminalKeyboardProtocol {
                    flags: crate::protocol::KittyKeyboardFlags::from_bits_retain(flags),
                    modify_other_keys_level: crate::ghostty::ModifyOtherKeysLevel::from_parameter(
                        u16::from(modify_other_keys_level),
                    ),
                }) {
                    Ok(framed) => framed,
                    Err(err) => {
                        warn!(err = %err, "failed to serialize direct terminal keyboard mode");
                        continue;
                    }
                };
            if writer.control.send(serialized).is_err() {
                debug!(
                    ?client_id,
                    "client writer channel closed during direct terminal keyboard update"
                );
                broken_clients.push(client_id);
                continue;
            }
            if let Some(state) = client.terminal_attach_state_mut() {
                state.host_keyboard_protocol_active = Some((flags, modify_other_keys_level));
            }
        }

        for client_id in broken_clients {
            self.remove_client_and_resize_if_needed(client_id);
        }
    }

    pub(super) fn has_pending_presentation_work(
        &self,
        render_demand: crate::api::RenderDemand,
    ) -> bool {
        render_demand == crate::api::RenderDemand::Full
            || self.app.render_dirty.has_immediate_work()
    }

    pub(super) fn sync_immediate_pty_sources(&self) {
        let (has_app_target, direct_terminal_targets) = self.pty_render_targets();
        let mut pane_ids = HashSet::new();
        if has_app_target {
            for (&client_id, client) in &self.clients {
                if !client.is_active_shell_client() || client.writer.is_none() {
                    continue;
                }
                let Some(target) = self.shell_target_for_client(client_id) else {
                    continue;
                };
                let Some((workspace_index, tab_index)) = target.resolve(&self.app.state) else {
                    continue;
                };
                let Some(tab) = self
                    .app
                    .state
                    .workspaces
                    .get(workspace_index)
                    .and_then(|workspace| workspace.tabs.get(tab_index))
                else {
                    continue;
                };
                if tab.zoomed {
                    pane_ids.insert(tab.layout.focused());
                } else {
                    pane_ids.extend(tab.layout.pane_ids());
                }
            }
        }
        if !direct_terminal_targets.is_empty() {
            for workspace in &self.app.state.workspaces {
                for tab in &workspace.tabs {
                    pane_ids.extend(tab.panes.iter().filter_map(|(&pane_id, pane)| {
                        direct_terminal_targets
                            .contains(pane.attached_terminal_id.as_str())
                            .then_some(pane_id)
                    }));
                }
            }
        }
        self.app.render_dirty.set_immediate_pty_sources(pane_ids);
    }

    fn pty_render_targets(&self) -> (bool, HashSet<&str>) {
        let mut has_app_target = false;
        let mut direct_terminal_targets = HashSet::new();
        for client in self
            .clients
            .values()
            .filter(|client| client.writer.is_some())
        {
            match &client.mode {
                ClientConnectionMode::ClientShell(shell) if shell.surface_active => {
                    has_app_target = true;
                }
                ClientConnectionMode::ClientShell(_) => {}
                ClientConnectionMode::TerminalAttach { terminal_id, .. } => {
                    direct_terminal_targets.insert(terminal_id.as_str());
                }
                ClientConnectionMode::TerminalPending => {}
            }
        }
        (has_app_target, direct_terminal_targets)
    }

    fn pty_source_visible_to_render_targets(
        &self,
        pane_id: crate::layout::PaneId,
        has_app_target: bool,
        direct_terminal_targets: &HashSet<&str>,
    ) -> bool {
        let terminal_id = self.terminal_id_for_pane(pane_id);
        (has_app_target && (terminal_id.is_none() || self.any_shell_surface_contains_pane(pane_id)))
            || terminal_id.is_none_or(|source| direct_terminal_targets.contains(source.as_str()))
    }

    pub(super) fn pty_sources_visible_to_any_render_target(
        &self,
        sources: &HashSet<crate::layout::PaneId>,
    ) -> bool {
        let (has_app_target, direct_terminal_targets) = self.pty_render_targets();
        if !has_app_target && direct_terminal_targets.is_empty() {
            return false;
        }

        sources.iter().copied().any(|pane_id| {
            self.pty_source_visible_to_render_targets(
                pane_id,
                has_app_target,
                &direct_terminal_targets,
            )
        })
    }

    fn terminal_id_for_pane(
        &self,
        pane_id: crate::layout::PaneId,
    ) -> Option<&crate::terminal::TerminalId> {
        self.app
            .find_pane(pane_id)
            .map(|(_, pane)| &pane.attached_terminal_id)
    }

    fn any_shell_surface_contains_pane(&self, pane_id: crate::layout::PaneId) -> bool {
        self.clients.iter().any(|(&client_id, client)| {
            if !client.is_active_shell_client() || client.writer.is_none() {
                return false;
            }
            let Some(target) = self.shell_target_for_client(client_id) else {
                return false;
            };
            let Some((workspace_index, tab_index)) = target.resolve(&self.app.state) else {
                return false;
            };
            let Some(tab) = self
                .app
                .state
                .workspaces
                .get(workspace_index)
                .and_then(|workspace| workspace.tabs.get(tab_index))
            else {
                return false;
            };
            tab.panes.contains_key(&pane_id) && (!tab.zoomed || tab.layout.focused() == pane_id)
        })
    }

    pub(super) fn render_and_stream(&mut self) {
        let render_targets = render_targets(&self.clients, self.clients.foreground_client_id());

        if render_targets.is_empty() {
            let (cols, rows) = (
                self.effective_size.cols.get(),
                self.effective_size.rows.get(),
            );
            let area = Rect::new(0, 0, cols, rows);
            let resize_panes = self.app.state.view.pane_infos.is_empty();
            crate::ui::compute_view_with_runtime_registry(
                &mut self.app.state,
                &self.app.terminal_runtimes,
                area,
            );
            if resize_panes {
                crate::ui::resize_all_tab_surfaces(
                    &self.app.state,
                    &self.app.terminal_runtimes,
                    area,
                    crate::host_term::cell_size::HostCellSize::default(),
                );
            }
            self.app.full_redraw_pending = false;
            debug!(
                cols,
                rows, resize_panes, "updated geometry with no attached clients"
            );
            return;
        }

        // Resize from the controlling client's geometry before drawing any observer.
        // Retained updates fall back here when a pane changes alternate screens.
        for target in &render_targets {
            let client_id = target.client_id;
            let (cols, rows) = (
                target.terminal_size.cols.get(),
                target.terminal_size.rows.get(),
            );
            let cell_size = target.cell_size;
            let Some(client) = self.clients.get(&client_id) else {
                continue;
            };
            if !client.is_active_shell_client() {
                continue;
            }
            let Some(surface_target) = self.shell_target_for_client(client_id) else {
                continue;
            };
            if self
                .clients
                .geometry_controller_by_id(&surface_target.tab_id)
                != Some(client_id)
            {
                continue;
            }
            let changed = client
                .render_state
                .last_pane_surface()
                .is_none_or(|surface| {
                    surface.panes.iter().any(|pane| {
                        let Some((workspace_index, pane_id)) =
                            self.app.parse_pane_id(&pane.pane_id)
                        else {
                            return false;
                        };
                        self.app
                            .state
                            .runtime_for_pane_in_workspace(
                                &self.app.terminal_runtimes,
                                workspace_index,
                                pane_id,
                            )
                            .is_some_and(|runtime| {
                                runtime.alternate_screen_active() != pane.alternate_screen_active
                            })
                    })
                });
            if changed && let Some(shell_target) = self.shell_target_for_client(client_id) {
                let area = Rect::new(0, 0, cols, rows);
                let layout = crate::ui::compute_tab_surface_for(
                    &self.app.state,
                    &self.app.terminal_runtimes,
                    Some(shell_target.clone()),
                    area,
                );
                let Some((workspace_index, _)) = shell_target.resolve(&self.app.state) else {
                    continue;
                };
                if layout.pane_infos.iter().any(|pane| {
                    self.app
                        .state
                        .runtime_for_pane_in_workspace(
                            &self.app.terminal_runtimes,
                            workspace_index,
                            pane.id,
                        )
                        .is_some_and(crate::terminal::TerminalRuntime::synchronized_output_active)
                }) {
                    continue;
                }
                crate::ui::resize_tab_surface_layout(
                    &self.app.state,
                    &self.app.terminal_runtimes,
                    &layout,
                    if cell_size.is_known() {
                        cell_size
                    } else {
                        crate::host_term::cell_size::HostCellSize::default()
                    },
                );
            }
        }

        let mut broken_clients: Vec<ClientId> = Vec::new();
        // The session snapshot every shell projection is diffed against is
        // built at most once per render and shared: copies for all but the
        // last shell client, the original for the last one.
        let mut shell_clients_left = render_targets
            .iter()
            .filter(|target| matches!(&target.mode, RenderTargetMode::Shell))
            .count();
        let mut shared_session_snapshot: Option<crate::api::schema::SessionSnapshot> = None;
        // (client, is shell client, claimed bytes, frame limit)
        let mut oversized_notices: Vec<(ClientId, bool, usize, usize)> = Vec::new();
        for target in render_targets {
            let client_id = target.client_id;
            let (cols, rows) = (
                target.terminal_size.cols.get(),
                target.terminal_size.rows.get(),
            );
            let cell_size = target.cell_size;
            let mode = target.mode;
            let is_shell = matches!(&mode, RenderTargetMode::Shell);
            let last_shell_client = if is_shell {
                shell_clients_left = shell_clients_left.saturating_sub(1);
                shell_clients_left == 0
            } else {
                false
            };
            let area = Rect::new(0, 0, cols, rows);
            let shell_target = self.shell_target_for_client(client_id);
            let shell_render = if is_shell
                && self
                    .clients
                    .get(&client_id)
                    .is_some_and(ClientConnection::is_active_shell_client)
            {
                let render_cell_size = if cell_size.is_known() {
                    cell_size
                } else {
                    crate::host_term::cell_size::HostCellSize::default()
                };
                let result = render_client_shell_pane_surface(
                    &mut self.app,
                    shell_target.as_ref(),
                    area,
                    render_cell_size,
                );
                match result {
                    Ok(surface) => Some(surface),
                    Err(reason) => {
                        if let Some(client) = self.clients.get_mut(&client_id) {
                            client.render_state.request_recompute();
                        }
                        if matches!(
                            reason,
                            crate::server::client_shell::SurfaceRenderDeferred::Changed
                        ) {
                            self.app.render_dirty.request_generic();
                        }
                        continue;
                    }
                }
            } else {
                None
            };
            let mut shell_projection_revision = crate::protocol::ProjectionRevision::ZERO;
            if is_shell {
                let session = if last_shell_client {
                    shared_session_snapshot
                        .take()
                        .unwrap_or_else(|| shell_session_snapshot(&self.app))
                } else {
                    shared_session_snapshot
                        .get_or_insert_with(|| shell_session_snapshot(&self.app))
                        .clone()
                };
                let Some(client) = self.clients.get_mut(&client_id) else {
                    continue;
                };
                let mut candidate = crate::server::client_shell::snapshot_from_session(
                    &self.app,
                    session,
                    &self.client_shell_boot_id,
                    client
                        .shell_state()
                        .map_or(0, |shell| shell.projection_revision.get()),
                    client
                        .shell_state()
                        .and_then(|shell| shell.location.as_ref()),
                );
                let snapshot_changed = client
                    .shell_state()
                    .is_some_and(|shell| shell.snapshot.as_ref() != Some(&candidate));
                if snapshot_changed {
                    let Some(shell) = client.shell_state_mut() else {
                        continue;
                    };
                    shell.projection_revision = shell.projection_revision.next();
                    candidate.revision = shell.projection_revision;
                    let snapshot_message = crate::protocol::endpoint::snapshot_message(&candidate);
                    let snapshot_framed = match Self::frame_server_message(&snapshot_message) {
                        Ok(framed) => framed,
                        Err(err) => {
                            warn!(?client_id, err = %err, "failed to frame endpoint snapshot");
                            broken_clients.push(client_id);
                            continue;
                        }
                    };
                    let Some(writer) = client.writer.as_ref().cloned() else {
                        broken_clients.push(client_id);
                        continue;
                    };
                    if writer.control.send(snapshot_framed).is_err() {
                        broken_clients.push(client_id);
                        continue;
                    }
                    if let Some(shell) = client.shell_state_mut() {
                        shell.snapshot = Some(candidate);
                    }
                }
                let Some(shell) = client.shell_state() else {
                    continue;
                };
                shell_projection_revision = shell.projection_revision;
                if !shell.surface_active {
                    client.clear_deferred_render();
                    continue;
                }
            }
            let mut surface_parts = None;
            let frame = match mode {
                RenderTargetMode::Shell => {
                    // Rendered above for every active shell client, and
                    // inactive ones were skipped just before this match, so
                    // there is always a surface here; without one there is
                    // nothing to send.
                    let Some(crate::server::client_shell::RenderedPaneSurface {
                        frame,
                        panes,
                        splits,
                    }) = shell_render
                    else {
                        continue;
                    };
                    surface_parts = Some((panes, splits));
                    frame
                }
                RenderTargetMode::TerminalAttach { terminal_id } => {
                    let Some(runtime) = self.app.terminal_runtimes.get(&terminal_id) else {
                        self.send_to_client(
                            client_id,
                            &ServerMessage::ServerShutdown {
                                reason: Some(crate::protocol::ShutdownReason::Message(format!(
                                    "terminal attach ended: terminal {terminal_id} not found"
                                ))),
                            },
                        );
                        broken_clients.push(client_id);
                        continue;
                    };
                    let (synchronized, epoch) = runtime.synchronized_output_state();
                    if synchronized {
                        if let Some(client) = self.clients.get_mut(&client_id) {
                            client.render_state.request_recompute();
                        }
                        continue;
                    }
                    let (buffer, cursor) =
                        crate::server::render_stream::render_terminal_virtual(runtime, area);
                    let hyperlinks = runtime.visible_hyperlinks(area);
                    let (synchronized, after_epoch) = runtime.synchronized_output_state();
                    if synchronized || after_epoch != epoch {
                        if let Some(client) = self.clients.get_mut(&client_id) {
                            client.render_state.request_recompute();
                        }
                        if !synchronized {
                            self.app.render_dirty.request_generic();
                        }
                        continue;
                    }
                    FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, cursor, &hyperlinks)
                }
            };

            let Some(client) = self.clients.get_mut(&client_id) else {
                continue;
            };
            let Some(writer) = client.writer.as_ref().cloned() else {
                continue;
            };
            let prepared = if let Some((panes, splits)) = surface_parts {
                client
                    .render_state
                    .prepare_pane_surface(protocol::PaneSurfaceFrame {
                        boot_id: self.client_shell_boot_id.clone(),
                        projection_revision: shell_projection_revision,
                        surface_revision: crate::protocol::SurfaceRevision::new(0),
                        frame,
                        panes,
                        splits,
                    })
            } else {
                client.render_state.prepare_frame(frame)
            };
            let Some(prepared) = prepared else {
                client.clear_deferred_render();
                continue;
            };
            let serialized = match Self::frame_server_message(prepared.message()) {
                Ok(frame) => frame,
                Err(protocol::FramingError::Oversized { claimed, max }) => {
                    // Nothing is committed, so the next render that has work
                    // for this client tries a full frame again: the frame fits
                    // again once the window shrinks or the content gets
                    // cheaper (fewer hyperlinks or long graphemes). Renders
                    // only run on real damage, so this does not spin. What
                    // must not happen is a client that stays blank with
                    // nobody told why, or a warning per render.
                    if client.oversized_frame_reported {
                        debug!(
                            ?client_id,
                            claimed, max, "skipping oversized frame for client"
                        );
                    } else {
                        warn!(
                            ?client_id,
                            claimed, max, "skipping oversized frame for client"
                        );
                        client.oversized_frame_reported = true;
                        oversized_notices.push((client_id, client.is_shell_client(), claimed, max));
                    }
                    continue;
                }
                Err(err) => {
                    warn!(?client_id, err = %err, "failed to serialize frame");
                    broken_clients.push(client_id);
                    continue;
                }
            };
            let send = writer.render.try_send(serialized);
            match send {
                Ok(()) => {
                    client.render_state.commit_sent_frame(prepared);
                    client.clear_deferred_render();
                    client.oversized_frame_reported = false;
                }
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    client.defer_full_render();
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    broken_clients.push(client_id);
                }
            }
        }

        for (client_id, shell, claimed, max) in oversized_notices {
            if broken_clients.contains(&client_id) {
                continue;
            }
            let notice = if shell {
                ServerMessage::ClientShellError {
                    kind: crate::protocol::NoticeKind::OversizedFrame { claimed, max },
                }
            } else {
                ServerMessage::DirectTerminalNotice {
                    kind: crate::protocol::NoticeKind::OversizedFrame { claimed, max },
                }
            };
            self.send_to_client(client_id, &notice);
        }

        if !broken_clients.is_empty() {
            for client_id in broken_clients {
                self.remove_client_and_resize_if_needed(client_id);
            }
        }

        let (cols, rows) = (
            self.effective_size.cols.get(),
            self.effective_size.rows.get(),
        );
        // Full-frame recovery is tracked per connection. A slow client must not
        // keep responsive peers on the global full-render path while it waits
        // for its render slot to drain.
        self.app.full_redraw_pending = false;
        debug!(cols, rows, foreground_client_id = ?self.clients.foreground_client_id(), "rendered virtual frame(s)");
    }
}
