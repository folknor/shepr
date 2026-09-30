use super::*;
use crate::server::ClientId;

fn writer_gone(client_id: ClientId) {
    debug!(?client_id, "client writer channel closed");
}

pub(super) use crate::limits::SHELL_CWD_REFRESH_INTERVAL;

/// The layout-free session snapshot every shell projection is built from,
/// shared by all shell clients.
pub(super) struct ShellSessionCache {
    /// `AppState::shell_projection_revision` this snapshot was built at.
    pub(super) revision: u64,
    /// When the snapshot last read the `/proc`-derived fields.
    pub(super) built_at: Instant,
    pub(super) session: crate::app::SessionSnapshot,
    /// Projections already checked by the cwd timer, available to the render
    /// pass that the first changed projection requests.
    pub(super) timer_projections: HashMap<ClientId, CachedShellProjection>,
}

pub(super) struct CachedShellProjection {
    location_generation: u64,
    projection_revision: u64,
    snapshot: shepr_protocol::ClientShellSnapshot,
}

type PaneSurfaceRenderKey = (Option<shepr_protocol::WorkspaceId>, u16, u16, u32, u32);

fn pane_surface_render_key(
    target: Option<&shepr_protocol::WorkspaceId>,
    area: Rect,
    cell_size: shepr_termio::host_term::cell_size::HostCellSize,
) -> PaneSurfaceRenderKey {
    let cell_size = cell_size.or_default();
    (
        target.cloned(),
        area.width,
        area.height,
        cell_size.width_px,
        cell_size.height_px,
    )
}

impl HeadlessServer {
    pub(super) fn shell_cwd_refresh_deadline(&self) -> Option<Instant> {
        self.clients.latest_shell_client()?;
        self.shell_session_cache
            .as_ref()
            .map(|cache| cache.built_at + SHELL_CWD_REFRESH_INTERVAL)
    }

    pub(super) fn shell_cwd_refresh_due(&self, now: Instant) -> bool {
        self.shell_cwd_refresh_deadline()
            .is_some_and(|deadline| deadline <= now)
    }

    fn rebuild_shell_session_cache(&mut self) {
        self.shell_session_cache = Some(ShellSessionCache {
            revision: self.app.state.shell_projection_revision,
            built_at: self.app.clock.now,
            session: self.app.session_snapshot(),
            timer_projections: HashMap::new(),
        });
    }

    /// Timer path for inputs no event reports. Rebuilds the shared session and
    /// checks clients until the first changed projection. That change requests
    /// a render, which reuses the projections already built on this pass. An
    /// idle server pays one session build and one projection per client each
    /// interval, not a surface render. This also bounds how long any missed
    /// invalidation can leave a client stale.
    pub(super) fn refresh_shell_projection_sources(&mut self) -> bool {
        self.rebuild_shell_session_cache();
        let Some(cache) = self.shell_session_cache.as_ref() else {
            return false;
        };
        let mut timer_projections = HashMap::new();
        let mut changed = false;
        for (&client_id, client) in &self.clients {
            let shell = client.shell_state();
            let candidate = crate::server::client_shell::snapshot_from_session(
                &self.app,
                &cache.session,
                &self.client_shell_boot_id,
                shell.projection_revision.get(),
                &shell.location,
            );
            let client_changed = shell
                .snapshot
                .as_ref()
                .is_none_or(|sent| candidate != *sent);
            timer_projections.insert(
                client_id,
                CachedShellProjection {
                    location_generation: shell.location.generation(),
                    projection_revision: shell.projection_revision.get(),
                    snapshot: candidate,
                },
            );
            if client_changed {
                changed = true;
                break;
            }
        }
        if changed {
            self.shell_session_generation = self.shell_session_generation.saturating_add(1);
            if let Some(cache) = self.shell_session_cache.as_mut() {
                cache.timer_projections = timer_projections;
            }
        }
        changed
    }

    fn shell_focused_runtime(
        &self,
        client_id: ClientId,
    ) -> Option<(&shepr_mux::pane::PaneRuntime, shepr_core::layout::PaneId)> {
        let target = self.shell_target_for_client(client_id)?;
        let workspace_index = self.app.state.workspace_index(&target)?;
        let pane_id = self
            .app
            .state
            .workspaces
            .get(workspace_index)?
            .focused_pane_id();
        self.app
            .state
            .runtime_for_pane_in_workspace(&self.app.terminal_runtimes, workspace_index, pane_id)
            .map(|runtime| (runtime, pane_id))
    }

    pub(super) fn stream_host_mouse_capture_mode(&mut self) {
        let requested = self
            .clients
            .iter()
            .map(|(&client_id, client)| {
                let shell = client.shell_state();
                let focused = shell
                    .surface_active
                    .then(|| self.shell_focused_runtime(client_id))
                    .flatten();
                let child_requests_mouse =
                    focused.is_some_and(|(runtime, _)| runtime.mouse_reporting_enabled());
                let sgr_pixels = client.pixel_mouse
                    && focused.is_some_and(|(runtime, _)| runtime.sgr_pixel_mouse_enabled());
                (
                    client_id,
                    shell.surface_active && (shell.mouse_capture || child_requests_mouse),
                    shell.surface_active && sgr_pixels,
                )
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
                    warn!(error = %err, "failed to serialize mouse capture mode for client");
                    continue;
                }
            };
            if writer.control.send(serialized).is_err() {
                writer_gone(client_id);
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

    pub(super) fn stream_shell_keyboard_mode(&mut self) {
        let shell_modes = self
            .clients
            .iter()
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
            if client.shell_state().host_keyboard_report_all_active == Some(report_all) {
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
                    warn!(error = %err, "failed to serialize client shell keyboard report-all mode");
                    continue;
                }
            };
            if writer.control.send(serialized).is_err() {
                writer_gone(client_id);
                broken_clients.push(client_id);
                continue;
            }
            client.shell_state_mut().host_keyboard_report_all_active = Some(report_all);
        }

        for client_id in broken_clients {
            self.remove_client_and_resize_if_needed(client_id);
        }
    }

    pub(super) fn has_pending_presentation_work(&self, render_demand: RenderDemand) -> bool {
        render_demand == RenderDemand::Full || self.app.render_dirty.has_immediate_work()
    }

    pub(super) fn sync_immediate_pty_sources(&self) {
        let mut pane_ids = HashSet::new();
        for (&client_id, client) in &self.clients {
            if !client.is_active_shell_client() || client.writer.is_none() {
                continue;
            }
            let Some(target) = self.shell_target_for_client(client_id) else {
                continue;
            };
            let Some(workspace) = self
                .app
                .state
                .workspace_index(&target)
                .and_then(|workspace_index| self.app.state.workspaces.get(workspace_index))
            else {
                continue;
            };
            if workspace.zoomed() {
                pane_ids.insert(workspace.focused_pane_id());
            } else {
                pane_ids.extend(workspace.layout().pane_ids());
            }
        }
        self.app.render_dirty.set_immediate_pty_sources(pane_ids);
    }

    pub(super) fn pty_sources_visible_to_any_render_target(
        &self,
        sources: &HashSet<shepr_core::layout::PaneId>,
    ) -> bool {
        if !self.has_app_client() {
            return false;
        }

        sources.iter().copied().any(|pane_id| {
            self.terminal_id_for_pane(pane_id).is_none()
                || self.any_shell_surface_contains_pane(pane_id)
        })
    }

    fn terminal_id_for_pane(
        &self,
        pane_id: shepr_core::layout::PaneId,
    ) -> Option<&shepr_protocol::TerminalId> {
        self.app
            .find_pane(pane_id)
            .map(|(_, pane)| &pane.attached_terminal_id)
    }

    fn any_shell_surface_contains_pane(&self, pane_id: shepr_core::layout::PaneId) -> bool {
        self.clients.iter().any(|(&client_id, client)| {
            if !client.is_active_shell_client() || client.writer.is_none() {
                return false;
            }
            let Some(target) = self.shell_target_for_client(client_id) else {
                return false;
            };
            self.app
                .state
                .workspace_index(&target)
                .and_then(|workspace_index| self.app.state.workspaces.get(workspace_index))
                .is_some_and(|workspace| workspace.shows_pane(pane_id))
        })
    }

    /// Whether a visible pane of the workspace `target` names is inside a
    /// synchronized update, which a resize would tear.
    fn workspace_has_synchronized_pane(&self, target: &shepr_protocol::WorkspaceId) -> bool {
        let Some(workspace_index) = self.app.state.workspace_index(target) else {
            return false;
        };
        let Some(workspace) = self.app.state.workspaces.get(workspace_index) else {
            return false;
        };
        let visible = if workspace.zoomed() {
            vec![workspace.focused_pane_id()]
        } else {
            workspace.layout().pane_ids()
        };
        visible.into_iter().any(|pane_id| {
            self.app
                .state
                .runtime_for_pane_in_workspace(
                    &self.app.terminal_runtimes,
                    workspace_index,
                    pane_id,
                )
                .is_some_and(shepr_mux::pane::PaneRuntime::synchronized_output_active)
        })
    }

    pub(super) fn render_and_stream(&mut self) {
        let render_targets = render_targets(&self.clients);

        if render_targets.is_empty() {
            // With nothing to draw, only geometry is due: a workspace the
            // server has not laid out yet (at startup, or created while no
            // client was attached) gets its PTY size from the PTY size rule.
            let laid_out =
                self.app.state.has_workspace_without_area() && self.apply_all_workspace_geometry();
            self.app.full_redraw_pending = false;
            if let Some(cache) = self.shell_session_cache.as_mut() {
                cache.timer_projections.clear();
            }
            debug!(laid_out, "updated geometry with no attached clients");
            return;
        }
        let render_target_count = render_targets.len();
        let mut remaining_surface_renders = HashMap::new();
        for target in &render_targets {
            let Some(client) = self.clients.get(&target.client_id) else {
                continue;
            };
            if !client.is_active_shell_client() {
                continue;
            }
            let area = Rect::new(
                0,
                0,
                target.terminal_size.cols.get(),
                target.terminal_size.rows.get(),
            );
            let shell_target = self.shell_target_for_client(target.client_id);
            let key = pane_surface_render_key(shell_target.as_ref(), area, target.cell_size);
            *remaining_surface_renders.entry(key).or_insert(0usize) += 1;
        }
        let mut shared_surface_renders = HashMap::new();

        // Resize a workspace from its geometry source before drawing any observer.
        // Retained updates fall back here when a pane changes alternate screens.
        for target in &render_targets {
            let client_id = target.client_id;
            let Some(client) = self.clients.get(&client_id) else {
                continue;
            };
            if !client.is_active_shell_client() {
                continue;
            }
            let Some(surface_target) = self.shell_target_for_client(client_id) else {
                continue;
            };
            if self.workspace_geometry_source(&surface_target)
                != Some(super::client_views::GeometrySource::Client(client_id))
            {
                continue;
            }
            let changed = client
                .render_state
                .last_pane_surface()
                .is_none_or(|surface| {
                    let identities = &client.surface_pane_identities;
                    if identities.len() != surface.panes.len() {
                        return true;
                    }
                    let Some(first_identity) = identities.first() else {
                        return false;
                    };
                    let Some(workspace_index) =
                        self.app.resolve_workspace_id(&first_identity.workspace_id)
                    else {
                        return false;
                    };
                    if identities
                        .iter()
                        .any(|identity| identity.workspace_id != first_identity.workspace_id)
                    {
                        return true;
                    }
                    surface
                        .panes
                        .iter()
                        .zip(identities)
                        .any(|(pane, identity)| {
                            self.app
                                .state
                                .runtime_for_pane_in_workspace(
                                    &self.app.terminal_runtimes,
                                    workspace_index,
                                    identity.pane_id,
                                )
                                .is_some_and(|runtime| {
                                    runtime.alternate_screen_active()
                                        != pane.alternate_screen_active
                                })
                        })
                });
            if !changed || self.workspace_has_synchronized_pane(&surface_target) {
                continue;
            }
            self.apply_workspace_geometry(&surface_target);
        }

        let mut broken_clients: Vec<ClientId> = Vec::new();
        // Rebuild the shared session only when application state that feeds
        // it changed. `/proc`-derived fields are rechecked by the headless
        // loop's timer (`refresh_shell_projection_sources`), not here.
        let app_revision = self.app.state.shell_projection_revision;
        let refresh_session = !render_targets.is_empty()
            && self
                .shell_session_cache
                .as_ref()
                .is_none_or(|cache| cache.revision != app_revision);
        if refresh_session {
            self.rebuild_shell_session_cache();
            self.shell_session_generation = self.shell_session_generation.saturating_add(1);
        }
        // (client, claimed bytes, message limit)
        let mut oversized_notices: Vec<(ClientId, usize, usize)> = Vec::new();
        for target in render_targets {
            let client_id = target.client_id;
            let (cols, rows) = (
                target.terminal_size.cols.get(),
                target.terminal_size.rows.get(),
            );
            let cell_size = target.cell_size;
            let area = Rect::new(0, 0, cols, rows);
            let shell_target = self.shell_target_for_client(client_id);
            let shell_render = if self
                .clients
                .get(&client_id)
                .is_some_and(ClientConnection::is_active_shell_client)
            {
                let render_cell_size = cell_size.or_default();
                let key = pane_surface_render_key(shell_target.as_ref(), area, render_cell_size);
                let remaining = remaining_surface_renders
                    .get_mut(&key)
                    .map_or(1, |remaining| {
                        let current = *remaining;
                        *remaining = remaining.saturating_sub(1);
                        current
                    });
                // Pane rendering reads shared terminal cores and produces the
                // same frame for clients with the same workspace and geometry.
                // Keep an Arc-backed result until the last matching client so
                // only the per-client wire surface has to own a frame copy.
                let result = if remaining == 1 {
                    shared_surface_renders.remove(&key).unwrap_or_else(|| {
                        render_client_shell_pane_surface(
                            &self.app,
                            shell_target.as_ref(),
                            area,
                            render_cell_size,
                        )
                    })
                } else if let Some(result) = shared_surface_renders.get(&key) {
                    result.clone()
                } else {
                    let result = render_client_shell_pane_surface(
                        &self.app,
                        shell_target.as_ref(),
                        area,
                        render_cell_size,
                    );
                    shared_surface_renders.insert(key, result.clone());
                    result
                };
                match result {
                    Ok(surface) => Some(surface),
                    Err(reason) => {
                        // Only the surface waits (synchronized output); the
                        // projection below still goes out, so a held
                        // endpoint reply flushed after this render never
                        // reaches the client ahead of the snapshot its
                        // command changed.
                        if let Some(client) = self.clients.get_mut(&client_id) {
                            client.render_state.request_recompute();
                        }
                        if matches!(
                            reason,
                            crate::server::client_shell::SurfaceRenderDeferred::Changed
                        ) {
                            self.app.render_dirty.request_generic();
                        }
                        None
                    }
                }
            } else {
                None
            };
            let Some(client) = self.clients.get_mut(&client_id) else {
                continue;
            };
            // A projection is due when the shared session moved, or when this
            // client's own location did: a location change invalidates only
            // the projection of the client that moved.
            let needs_projection = {
                let shell = client.shell_state();
                shell.session_generation != self.shell_session_generation
                    || shell.projected_location_generation != shell.location.generation()
                    || shell.snapshot.is_none()
            };
            if needs_projection {
                let (location_generation, projection_revision) = {
                    let shell = client.shell_state();
                    (shell.location.generation(), shell.projection_revision.get())
                };
                let cached_projection = self
                    .shell_session_cache
                    .as_mut()
                    .and_then(|cache| cache.timer_projections.remove(&client_id))
                    .filter(|candidate| {
                        candidate.location_generation == location_generation
                            && candidate.projection_revision == projection_revision
                    });
                let mut candidate = if let Some(cached) = cached_projection {
                    cached.snapshot
                } else {
                    let Some(cache) = self.shell_session_cache.as_ref() else {
                        continue;
                    };
                    crate::server::client_shell::snapshot_from_session(
                        &self.app,
                        &cache.session,
                        &self.client_shell_boot_id,
                        projection_revision,
                        &client.shell_state().location,
                    )
                };
                let snapshot_changed = client.shell_state().snapshot.as_ref() != Some(&candidate);
                if snapshot_changed {
                    let shell = client.shell_state_mut();
                    // The counter is per connection and steps once per
                    // changed snapshot, so exhaustion is unreachable in
                    // practice. Should it happen, drop the client: it
                    // reconnects with a fresh counter instead of receiving
                    // a snapshot that repeats a revision.
                    let Some(revision) = shell.projection_revision.checked_next() else {
                        warn!(
                            ?client_id,
                            "projection revisions exhausted; dropping client"
                        );
                        broken_clients.push(client_id);
                        continue;
                    };
                    shell.projection_revision = revision;
                    candidate.revision = revision;
                    let snapshot_message = shepr_protocol::endpoint::snapshot_message(&candidate);
                    let snapshot_framed = match Self::frame_server_message(&snapshot_message) {
                        Ok(framed) => framed,
                        Err(err) => {
                            warn!(?client_id, error = %err, "failed to frame endpoint snapshot");
                            broken_clients.push(client_id);
                            continue;
                        }
                    };
                    let Some(writer) = client.writer.as_ref().cloned() else {
                        broken_clients.push(client_id);
                        continue;
                    };
                    if writer.control.send(snapshot_framed).is_err() {
                        writer_gone(client_id);
                        broken_clients.push(client_id);
                        continue;
                    }
                    client.shell_state_mut().snapshot = Some(candidate);
                }
                // Only a projection that succeeded advances what was projected.
                let shell = client.shell_state_mut();
                shell.session_generation = self.shell_session_generation;
                shell.projected_location_generation = shell.location.generation();
            }
            let shell = client.shell_state();
            let shell_projection_revision = shell.projection_revision;
            if !shell.surface_active {
                client.clear_deferred_render();
                continue;
            }
            // Rendered above for every active shell client, and inactive ones
            // were skipped just before this, so there is always a surface
            // here; without one there is nothing to send.
            let Some(crate::server::client_shell::RenderedPaneSurface {
                frame,
                panes,
                splits,
                pane_identities,
            }) = shell_render
            else {
                continue;
            };
            let frame =
                std::sync::Arc::try_unwrap(frame).unwrap_or_else(|shared| shared.as_ref().clone());

            // A public pane ID can outlive a layout update with no change to
            // the wire fields, but its internal pane identity still belongs
            // in the committed baseline used by retained rendering.
            if client.surface_pane_identities != pane_identities {
                client.render_state.request_recompute();
            }

            let Some(writer) = client.writer.as_ref().cloned() else {
                continue;
            };
            let prepared =
                client
                    .render_state
                    .prepare_pane_surface(shepr_protocol::PaneSurfaceFrame {
                        boot_id: self.client_shell_boot_id.clone(),
                        projection_revision: shell_projection_revision,
                        surface_revision: shepr_protocol::SurfaceRevision::new(0),
                        frame,
                        panes,
                        splits,
                    });
            let Some(prepared) = prepared else {
                client.clear_deferred_render();
                continue;
            };
            // A surface past one frame is split across frames here; only one
            // past `MAX_MESSAGE_SIZE` fails.
            let serialized = match Self::frame_server_message(prepared.message()) {
                Ok(frame) => frame,
                Err(shepr_protocol::FramingError::Oversized { claimed, max }) => {
                    // Nothing is committed, so the next render that has work
                    // for this client tries a full surface again: it fits
                    // again once the window shrinks or the content gets
                    // cheaper (fewer hyperlinks or long graphemes). Renders
                    // only run on real damage, so this does not spin. What
                    // must not happen is a client that stays blank with
                    // nobody told why, or a warning per render.
                    if client.oversized_surface_reported {
                        debug!(
                            ?client_id,
                            claimed, max, "skipping oversized surface for client"
                        );
                    } else {
                        warn!(
                            ?client_id,
                            claimed, max, "skipping oversized surface for client"
                        );
                        client.oversized_surface_reported = true;
                        oversized_notices.push((client_id, claimed, max));
                    }
                    continue;
                }
                Err(err) => {
                    warn!(?client_id, error = %err, "failed to serialize frame");
                    broken_clients.push(client_id);
                    continue;
                }
            };
            let send = writer.render.try_send(serialized);
            match send {
                Ok(()) => {
                    client.render_state.commit_sent_frame(prepared);
                    client.commit_surface_pane_identities(pane_identities);
                    client.clear_deferred_render();
                    client.oversized_surface_reported = false;
                }
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    client.defer_full_render();
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    broken_clients.push(client_id);
                }
            }
        }

        for (client_id, claimed, max) in oversized_notices {
            if broken_clients.contains(&client_id) {
                continue;
            }
            let notice = ServerMessage::ClientShellError {
                kind: shepr_protocol::NoticeKind::OversizedSurface { claimed, max },
            };
            self.send_to_client(client_id, &notice);
        }

        if !broken_clients.is_empty() {
            for client_id in broken_clients {
                self.remove_client_and_resize_if_needed(client_id);
            }
        }
        if let Some(cache) = self.shell_session_cache.as_mut() {
            cache.timer_projections.clear();
        }

        // Full-frame recovery is tracked per connection. A slow client must not
        // keep responsive peers on the global full-render path while it waits
        // for its render slot to drain.
        self.app.full_redraw_pending = false;
        debug!(targets = render_target_count, "rendered virtual frame(s)");
    }
}
