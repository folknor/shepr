use super::*;
use crate::server::ClientId;
use crate::server::render_stream::{PreparedSurface, ViewEpoch};

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

/// What one loop iteration owes, derived by `render_plan`.
pub(super) struct RenderPlan {
    /// Clients due a projection or a surface, in ascending id order.
    pub(super) full: Vec<ClientId>,
    /// Clients with a current baseline that can take pending PTY damage as a
    /// retained patch.
    pub(super) patch: Vec<ClientId>,
    /// With no client attached, whether the PTY size rule is due for this
    /// epoch.
    pub(super) headless_geometry: bool,
}
impl RenderPlan {
    pub(super) fn has_full(&self) -> bool {
        !self.full.is_empty() || self.headless_geometry
    }
}

/// What one `render_pass` did, for the debug log and tests.
#[derive(Default, Debug)]
pub(super) struct PassReport {
    /// Clients that went through the full step (projection and surface
    /// decision), including patch candidates promoted to it.
    pub(super) full: Vec<ClientId>,
    /// Clients that took a retained patch.
    pub(super) patched: Vec<ClientId>,
    /// Clients that could not take a surface this pass and are owed one.
    pub(super) owed: Vec<ClientId>,
    /// Surface renders made; clients sharing a workspace and size share one.
    pub(super) surface_renders: usize,
}
type SurfaceEncoder = fn(&ServerMessage) -> Result<Vec<u8>, shepr_protocol::FramingError>;

type SurfaceRenderer = fn(
    &app::App,
    Option<&shepr_protocol::WorkspaceId>,
    Rect,
    shepr_termio::host_term::cell_size::HostCellSize,
) -> Result<
    crate::server::client_shell::RenderedPaneSurface,
    crate::server::client_shell::SurfaceRenderDeferred,
>;

pub(super) struct SurfaceBoundary {
    pub(super) encode: SurfaceEncoder,
    pub(super) render: SurfaceRenderer,
}
impl Default for SurfaceBoundary {
    fn default() -> Self {
        Self {
            encode: shepr_protocol::encode_message,
            render: render_client_shell_pane_surface,
        }
    }
}

struct SharedSurfaces {
    boundary: SurfaceBoundary,
    remaining: HashMap<PaneSurfaceRenderKey, usize>,
    rendered: HashMap<
        PaneSurfaceRenderKey,
        Result<
            crate::server::client_shell::RenderedPaneSurface,
            crate::server::client_shell::SurfaceRenderDeferred,
        >,
    >,
    surface_renders: usize,
    oversized_notices: Vec<(ClientId, usize, usize)>,
}

/// How one client's full step ended; `render_full` maps it to the client's
/// surface debt in one place.
enum ClientPassOutcome {
    /// A surface was queued in its slot.
    Delivered,
    /// The surface equals its baseline: nothing to send, nothing owed.
    Unchanged,
    /// It could not take a surface this pass.
    Owed,
    /// Its surface is too large to send.
    Refused,
    /// An inactive shell: projected only.
    Skipped,
    /// Its outbox closed; the reap removes it.
    Closed,
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

    /// Rebuilds the shared session when application state that feeds it has
    /// changed, so a render and a new connection's seed both project from the
    /// one current source. A rebuild advances the generation, so existing
    /// clients see that change too. Afterwards the cache is always present.
    pub(super) fn refresh_stale_shell_session_cache(&mut self) {
        let app_revision = self.app.state.shell_projection_revision;
        let cache_is_current = self
            .shell_session_cache
            .as_ref()
            .is_some_and(|cache| cache.revision == app_revision);
        if !cache_is_current {
            self.rebuild_shell_session_cache();
            self.shell_session_generation = self.shell_session_generation.saturating_add(1);
        }
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
                let presenting = self.clients.is_presenting(&client_id);
                let focused = presenting
                    .then(|| self.shell_focused_runtime(client_id))
                    .flatten();
                let child_requests_mouse =
                    focused.is_some_and(|(runtime, _)| runtime.mouse_reporting_enabled());
                let sgr_pixels = client.pixel_mouse
                    && focused.is_some_and(|(runtime, _)| runtime.sgr_pixel_mouse_enabled());
                (
                    client_id,
                    presenting && (shell.mouse_capture || child_requests_mouse),
                    presenting && sgr_pixels,
                )
            })
            .collect::<Vec<_>>();

        for (client_id, enabled, sgr_pixels) in requested {
            if let Some(client) = self.clients.get_mut(&client_id) {
                client.outbox.tell_mouse_capture(enabled, sgr_pixels);
            }
        }
    }

    pub(super) fn stream_shell_keyboard_mode(&mut self) {
        let shell_modes = self
            .clients
            .iter()
            .map(|(&client_id, _)| {
                let report_all = self.clients.is_presenting(&client_id)
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
        for (client_id, report_all) in shell_modes {
            if let Some(client) = self.clients.get_mut(&client_id) {
                client.outbox.tell_keyboard_report_all(report_all);
            }
        }
    }

    pub(super) fn sync_immediate_pty_sources(&self) {
        let mut pane_ids = HashSet::new();
        for (&client_id, _) in &self.clients {
            if !self.clients.is_presenting(&client_id) {
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
            pane_ids.extend(workspace.visible_pane_ids());
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
        self.clients.iter().any(|(&client_id, _)| {
            if !self.clients.is_presenting(&client_id) {
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

    /// The runtimes of the panes a surface of the workspace `target` names
    /// shows: the focused pane when zoomed, every layout pane otherwise.
    fn visible_pane_runtimes(
        &self,
        target: &shepr_protocol::WorkspaceId,
    ) -> Vec<&shepr_mux::pane::PaneRuntime> {
        let Some(workspace_index) = self.app.state.workspace_index(target) else {
            return Vec::new();
        };
        let Some(workspace) = self.app.state.workspaces.get(workspace_index) else {
            return Vec::new();
        };
        workspace
            .visible_pane_ids()
            .into_iter()
            .filter_map(|pane_id| {
                self.app.state.runtime_for_pane_in_workspace(
                    &self.app.terminal_runtimes,
                    workspace_index,
                    pane_id,
                )
            })
            .collect()
    }

    /// Whether a visible pane of the workspace `target` names is inside a
    /// synchronized update, which a resize would tear.
    fn workspace_has_synchronized_pane(&self, target: &shepr_protocol::WorkspaceId) -> bool {
        self.visible_pane_runtimes(target)
            .into_iter()
            .any(shepr_mux::pane::PaneRuntime::synchronized_output_active)
    }

    /// What each attached client is owed this iteration, derived from its own
    /// settle point, location, baseline and slot plus the server-wide epoch.
    /// The checks run cheapest first: a client already due for a full pass
    /// never reaches `surface_deliverable`, which may lock terminal cores.
    pub(super) fn render_plan(&self, render_signal_pending: bool) -> RenderPlan {
        let targets = render_targets(&self.clients);
        let mut plan = RenderPlan {
            full: Vec::new(),
            patch: Vec::new(),
            headless_geometry: targets.is_empty() && self.headless_settled != self.view_epoch,
        };
        let mut held = HashMap::new();
        for target in targets {
            let Some(client) = self.clients.get(&target.client_id) else {
                continue;
            };
            let shell = client.shell_state();
            let full = !client.render_state.is_settled_at(self.view_epoch)
                || shell.projected_location_generation != shell.location.generation()
                || shell.snapshot.is_none()
                || (client.presents_surface()
                    && client.render_state.surface_debt()
                    && self.surface_deliverable(target.client_id, &mut held));
            if full {
                plan.full.push(target.client_id);
            } else if render_signal_pending
                && client.presents_surface()
                && client.render_state.takes_patches()
            {
                plan.patch.push(target.client_id);
            }
        }
        plan
    }

    /// Whether no surface of the workspace `target` names can be drawn now: a
    /// visible pane is inside a synchronized update, or its terminal core is
    /// poisoned. Either clears by itself (the update ends with a PTY repaint
    /// signal, a poisoned pane's actor closes it), so a client held here
    /// stays out of the plan rather than retrying.
    fn workspace_surface_held(&self, target: &shepr_protocol::WorkspaceId) -> bool {
        self.visible_pane_runtimes(target)
            .into_iter()
            .any(|runtime| {
                runtime
                    .synchronized_output_state()
                    .is_none_or(|(active, _)| active)
            })
    }

    /// The visible panes' PTY grid sizes, to tell whether a geometry
    /// application resized any of them.
    fn visible_pane_grid_sizes(
        &self,
        target: &shepr_protocol::WorkspaceId,
    ) -> Vec<shepr_core::geometry::GridSize> {
        self.visible_pane_runtimes(target)
            .into_iter()
            .map(shepr_mux::pane::PaneRuntime::grid_size)
            .collect()
    }

    /// Whether `id` can take a surface now: its slot is free and the
    /// workspace it views is not held. `held` memoizes the workspace check
    /// for one plan or step, so each workspace's cores are locked once.
    fn surface_deliverable(
        &self,
        id: ClientId,
        held: &mut HashMap<shepr_protocol::WorkspaceId, bool>,
    ) -> bool {
        let Some(client) = self.clients.get(&id) else {
            return false;
        };
        if !client.outbox.surface_slot_free() {
            return false;
        }
        let Some(workspace) = self.shell_target_for_client(id) else {
            return true;
        };
        !*held
            .entry(workspace.clone())
            .or_insert_with(|| self.workspace_surface_held(&workspace))
    }

    pub(super) fn render_pass(
        &mut self,
        plan: &RenderPlan,
        sources: &HashSet<shepr_core::layout::PaneId>,
    ) -> PassReport {
        self.render_pass_with_boundary(plan, sources, SurfaceBoundary::default())
    }

    // Inject the framing boundary so failure recovery can be verified without
    // constructing a surface larger than the protocol's one GiB limit.
    pub(super) fn render_pass_with_boundary(
        &mut self,
        plan: &RenderPlan,
        sources: &HashSet<shepr_core::layout::PaneId>,
        boundary: SurfaceBoundary,
    ) -> PassReport {
        let epoch = self.view_epoch;
        let mut report = PassReport::default();
        let mut full = plan.full.clone();
        if !sources.is_empty() {
            if self.app.state.settings.reveal_hidden_cursor_for_cjk_ime {
                full.extend(&plan.patch);
            } else {
                let patches = self.render_patches(&plan.patch, sources);
                full.extend(patches.promote);
                report.patched = patches.sent;
                report.owed = patches.owed;
                for id in &report.patched {
                    if let Some(client) = self.clients.get_mut(id) {
                        client.render_state.settle(epoch);
                    }
                }
            }
        }
        full.sort_unstable();
        full.dedup();
        self.render_full(&full, epoch, &mut report, boundary);
        if plan.headless_geometry {
            if self.app.state.has_workspace_without_area() {
                self.apply_all_workspace_geometry();
            }
            self.headless_settled = epoch;
        }
        if let Some(cache) = self.shell_session_cache.as_mut() {
            cache.timer_projections.clear();
        }
        debug!(?report, "rendered client surfaces");
        report
    }

    fn render_full(
        &mut self,
        ids: &[ClientId],
        epoch: ViewEpoch,
        report: &mut PassReport,
        boundary: SurfaceBoundary,
    ) {
        let render_targets = render_targets(&self.clients)
            .into_iter()
            .filter(|target| ids.contains(&target.client_id))
            .collect::<Vec<_>>();
        let mut held = HashMap::new();
        let deliverable = ids
            .iter()
            .map(|&id| (id, self.surface_deliverable(id, &mut held)))
            .collect::<HashMap<_, _>>();
        let mut shared = SharedSurfaces {
            boundary,
            remaining: HashMap::new(),
            rendered: HashMap::new(),
            surface_renders: 0,
            oversized_notices: Vec::new(),
        };
        for target in &render_targets {
            let Some(client) = self.clients.get(&target.client_id) else {
                continue;
            };
            if !client.presents_surface()
                || !deliverable.get(&target.client_id).copied().unwrap_or(false)
            {
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
            *shared.remaining.entry(key).or_insert(0usize) += 1;
        }

        // Resize a workspace from its geometry source before drawing any observer.
        // Retained updates fall back here when a pane changes alternate screens.
        let workspaces = ids
            .iter()
            .filter_map(|&id| self.shell_target_for_client(id))
            .collect::<HashSet<_>>();
        for surface_target in workspaces {
            let Some(super::client_views::GeometrySource::Client(client_id)) =
                self.workspace_geometry_source(&surface_target)
            else {
                continue;
            };
            let Some(client) = self.clients.get(&client_id) else {
                continue;
            };
            if !client.presents_surface() {
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
                    if first_identity.workspace_id != surface_target {
                        return true;
                    }
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
            // The outer workspace area can stay equal while an alternate
            // screen changes its panes' PTY sizes, so both are compared. A
            // resize reaches viewers outside this pass on the next iteration.
            // An application that resized nothing invalidates nobody: the
            // source's baseline can stay behind (its slot is busy, or its
            // surface was refused), and invalidating its co-viewers on every
            // pass would hand the pass back and forth between them.
            let sizes_before = self.visible_pane_grid_sizes(&surface_target);
            let area_changed = self.apply_workspace_geometry(&surface_target);
            if !area_changed && self.visible_pane_grid_sizes(&surface_target) == sizes_before {
                continue;
            }
            let viewers = self
                .clients
                .keys()
                .copied()
                .filter(|id| {
                    !ids.contains(id)
                        && self.shell_target_for_client(*id).as_ref() == Some(&surface_target)
                })
                .collect::<Vec<_>>();
            for id in viewers {
                if let Some(client) = self.clients.get_mut(&id) {
                    client.render_state.invalidate();
                }
            }
        }

        // Rebuild the shared session only when application state that feeds
        // it changed. `/proc`-derived fields are rechecked by the headless
        // loop's timer (`refresh_shell_projection_sources`), not here.
        if !render_targets.is_empty() {
            self.refresh_stale_shell_session_cache();
        }
        for target in render_targets {
            let client_id = target.client_id;
            let takes_surface = deliverable.get(&client_id).copied().unwrap_or(false);
            let outcome = self.render_client_full(&target, takes_surface, &mut shared);
            report.full.push(client_id);
            // The single exit: every outcome but a closed outbox maps to the
            // client's debt and settles it at the pass epoch, so no path can
            // leave a client stale for the next plan to pick up again.
            if let Some(client) = self.clients.get_mut(&client_id) {
                match outcome {
                    ClientPassOutcome::Closed => continue,
                    ClientPassOutcome::Owed => {
                        client.render_state.owe();
                        report.owed.push(client_id);
                    }
                    ClientPassOutcome::Refused => client.render_state.refuse(),
                    ClientPassOutcome::Delivered
                    | ClientPassOutcome::Unchanged
                    | ClientPassOutcome::Skipped => client.render_state.clear_debt(),
                }
                client.render_state.settle(epoch);
            }
        }

        report.surface_renders += shared.surface_renders;
        for (client_id, claimed, max) in shared.oversized_notices {
            let notice = ServerMessage::ClientShellError {
                kind: shepr_protocol::NoticeKind::OversizedSurface { claimed, max },
            };
            self.send_to_client(client_id, &notice);
        }
    }
    fn render_client_full(
        &mut self,
        target: &crate::server::clients::RenderTarget,
        deliverable: bool,
        shared: &mut SharedSurfaces,
    ) -> ClientPassOutcome {
        let client_id = target.client_id;
        let (cols, rows) = (
            target.terminal_size.cols.get(),
            target.terminal_size.rows.get(),
        );
        let cell_size = target.cell_size;
        let area = Rect::new(0, 0, cols, rows);
        let shell_target = self.shell_target_for_client(client_id);
        let Some(client) = self.clients.get_mut(&client_id) else {
            return ClientPassOutcome::Closed;
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
                // The step refreshed the cache before any client, which
                // leaves it present. Were it missing, the client is owed and
                // settled rather than reported closed: a closed outcome
                // skips the settle and, with its outbox still open, nothing
                // would ever reap it.
                let Some(cache) = self.shell_session_cache.as_ref() else {
                    warn!(?client_id, "shell session cache missing while projecting");
                    return ClientPassOutcome::Owed;
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
                    client.outbox.close();
                    return ClientPassOutcome::Closed;
                };
                shell.projection_revision = revision;
                candidate.revision = revision;
                let snapshot_message = shepr_protocol::endpoint::snapshot_message(&candidate);
                // A refused send has already closed the outbox.
                if client.outbox.send(&snapshot_message) == Delivery::Closed {
                    return ClientPassOutcome::Closed;
                }
                client.shell_state_mut().snapshot = Some(candidate);
            }
            // Only a projection that succeeded advances what was projected.
            let shell = client.shell_state_mut();
            shell.session_generation = self.shell_session_generation;
            shell.projected_location_generation = shell.location.generation();
        }
        let shell_projection_revision = client.shell_state().projection_revision;
        if !client.presents_surface() {
            return ClientPassOutcome::Skipped;
        }
        // A client that cannot take a surface now was projected above and
        // costs no surface render; its plan brings it back once it can.
        if !deliverable {
            return ClientPassOutcome::Owed;
        }
        let shell_render = {
            let render_cell_size = cell_size.or_default();
            let key = pane_surface_render_key(shell_target.as_ref(), area, render_cell_size);
            let remaining = shared.remaining.get_mut(&key).map_or(1, |remaining| {
                let current = *remaining;
                *remaining = remaining.saturating_sub(1);
                current
            });
            // Pane rendering reads shared terminal cores and produces the
            // same frame for clients with the same workspace and geometry.
            // Keep an Arc-backed result until the last matching client so
            // only the per-client wire surface has to own a frame copy.
            let result = if remaining == 1 {
                shared.rendered.remove(&key).unwrap_or_else(|| {
                    shared.surface_renders += 1;
                    (shared.boundary.render)(
                        &self.app,
                        shell_target.as_ref(),
                        area,
                        render_cell_size,
                    )
                })
            } else if let Some(result) = shared.rendered.get(&key) {
                result.clone()
            } else {
                shared.surface_renders += 1;
                let result = (shared.boundary.render)(
                    &self.app,
                    shell_target.as_ref(),
                    area,
                    render_cell_size,
                );
                shared.rendered.insert(key, result.clone());
                result
            };
            result.ok()
        };
        let Some(client) = self.clients.get_mut(&client_id) else {
            return ClientPassOutcome::Closed;
        };
        if shell_render.is_none() {
            client.render_state.request_recompute();
        }
        // Rendering is deferred while a pane is synchronized or its core is
        // poisoned (both appearing after the deliverability check), or when
        // the viewed workspace or a pane's content moved during the draw.
        // Only the surface waits: the projection already went out, so a held
        // endpoint reply released after this pass never reaches the client
        // ahead of the snapshot its command changed. The client is owed; a
        // moved pane raised its own render signal, and a vanished workspace
        // moved the view epoch, so nothing more is requested here.
        let Some(crate::server::client_shell::RenderedPaneSurface {
            frame,
            panes,
            splits,
            pane_identities,
        }) = shell_render
        else {
            return ClientPassOutcome::Owed;
        };
        // This connection's baseline advances only after its own send.
        // FrameData owns a Vec, so the shared render result needs an owned
        // cell grid for every connection that keeps a baseline.
        let frame =
            std::sync::Arc::try_unwrap(frame).unwrap_or_else(|shared| shared.as_ref().clone());

        // A public pane ID can outlive a layout update with no change to
        // the wire fields, but its internal pane identity still belongs
        // in the committed baseline used by retained rendering.
        if client.surface_pane_identities != pane_identities {
            client.render_state.request_recompute();
        }

        let prepared = client
            .render_state
            .prepare_pane_surface(shepr_protocol::PaneSurfaceFrame {
                boot_id: self.client_shell_boot_id.clone(),
                projection_revision: shell_projection_revision,
                surface_revision: shepr_protocol::SurfaceRevision::new(0),
                frame,
                panes,
                splits,
            });
        let prepared = match prepared {
            PreparedSurface::Ready(prepared) => *prepared,
            PreparedSurface::Unchanged => return ClientPassOutcome::Unchanged,
            PreparedSurface::RevisionsExhausted => {
                warn!(?client_id, "surface revisions exhausted; dropping client");
                client.outbox.close();
                return ClientPassOutcome::Closed;
            }
        };
        // A surface past one frame is split across frames here; only one
        // past `MAX_MESSAGE_SIZE` fails.
        let serialized = match (shared.boundary.encode)(prepared.message()) {
            Ok(frame) => frame,
            Err(shepr_protocol::FramingError::Oversized { claimed, max }) => {
                // Nothing is committed. The client is refused, which hides
                // the debt its missing or stale baseline implies, so it stays
                // out of the plan instead of retrying every pass. A new
                // epoch, its own resize or navigation, or PTY damage on a
                // pane it shows brings it back for a full surface, which fits
                // again once the window shrinks or the content gets cheaper.
                // The client is told once, not per render.
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
                    shared.oversized_notices.push((client_id, claimed, max));
                }
                return ClientPassOutcome::Refused;
            }
            Err(err) => {
                warn!(?client_id, error = %err, "failed to serialize frame");
                client.outbox.close();
                return ClientPassOutcome::Closed;
            }
        };
        match client.outbox.offer_surface(serialized) {
            crate::server::outbox::SurfaceOffer::Queued => {
                client.render_state.commit_sent_frame(prepared);
                client.commit_surface_pane_identities(pane_identities);
                client.oversized_surface_reported = false;
                ClientPassOutcome::Delivered
            }
            // Deliverability was checked before the step and the writer only
            // ever empties the slot, so this is a bug guard: owe and drop.
            crate::server::outbox::SurfaceOffer::Occupied => ClientPassOutcome::Owed,
            crate::server::outbox::SurfaceOffer::Closed => ClientPassOutcome::Closed,
        }
    }
}
