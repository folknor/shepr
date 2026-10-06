use super::client_views::ViewedWorkspace;
use super::*;
use crate::limits::SHELL_CWD_REFRESH_INTERVAL;
use crate::server::ClientId;
use crate::server::render_stream::{PreparedSurface, ViewEpoch};
use shepr_term::mouse::HostMouseCapture;

/// The layout-free session snapshot every shell projection is built from,
/// shared by all shell clients.
pub(super) struct ShellSessionCache {
    /// `AppState::shell_projection_revision` this snapshot was built at.
    pub(super) revision: shepr_protocol::ProjectionRevision,
    /// When the snapshot last read the `/proc`-derived fields.
    pub(super) built_at: Instant,
    pub(super) session: crate::app::ProjectionInput,
    /// Projections already checked by the cwd timer, available to the render
    /// pass that the first changed projection requests.
    pub(super) timer_projections: HashMap<ClientId, CachedShellProjection>,
}

pub(super) struct CachedShellProjection {
    location_generation: crate::server::clients::ClientShellLocationGeneration,
    projection_revision: shepr_protocol::ProjectionRevision,
    snapshot: shepr_protocol::ClientShellSnapshot,
}

type PaneSurfaceRenderKey = (Option<shepr_protocol::WorkspaceId>, u16, u16);

fn pane_surface_render_key(
    target: Option<crate::ui::SurfaceTarget>,
    area: shepr_core::geometry::Rect,
) -> PaneSurfaceRenderKey {
    (target.map(|target| target.id), area.width, area.height)
}

/// One client of a full render step, resolved once for the pass: where it
/// draws, the workspace it views and whether it can take a surface now.
struct PassClient {
    target: crate::server::clients::RenderTarget,
    view: Option<crate::ui::SurfaceTarget>,
    deliverable: bool,
}

/// What one loop iteration owes, derived by `render_plan`.
pub(super) struct RenderPlan {
    /// Clients due a projection or a surface, in ascending id order.
    pub(super) full: Vec<ClientId>,
    /// Clients with a current baseline that can take pending PTY damage as a
    /// retained patch.
    pub(super) patch: Vec<ClientId>,
}
impl RenderPlan {
    pub(super) fn has_full(&self) -> bool {
        !self.full.is_empty()
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
pub(super) trait SurfaceBoundary {
    fn encode(&self, message: &ServerMessage) -> Result<Vec<u8>, shepr_protocol::FramingError> {
        shepr_protocol::encode_message(message)
    }

    fn render(
        &self,
        app: &app::App,
        workspace: Option<crate::ui::SurfaceTarget>,
        area: shepr_core::geometry::Rect,
    ) -> Result<
        crate::server::pane_surface::RenderedPaneSurface,
        crate::server::pane_surface::SurfaceRenderDeferred,
    > {
        render_client_shell_pane_surface(app, workspace, area)
    }
}

struct LiveSurfaceBoundary;

impl SurfaceBoundary for LiveSurfaceBoundary {}

struct SharedSurfaces<B: SurfaceBoundary> {
    boundary: B,
    remaining: HashMap<PaneSurfaceRenderKey, usize>,
    rendered: HashMap<
        PaneSurfaceRenderKey,
        Result<
            crate::server::pane_surface::RenderedPaneSurface,
            crate::server::pane_surface::SurfaceRenderDeferred,
        >,
    >,
    surface_renders: usize,
    oversized_notices: Vec<OversizedNotice>,
}

/// A client whose surface was too large to send, to be told once.
struct OversizedNotice {
    client_id: ClientId,
    /// The surface message's size.
    claimed: usize,
    /// The size limit it exceeded.
    max: usize,
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
            revision: self.app.state().shell_projection_revision(),
            built_at: self.app.clock().now,
            session: self.app.projection_input(),
            timer_projections: HashMap::new(),
        });
    }

    /// Rebuilds the shared session when application state that feeds it has
    /// changed, so a render and a new connection's seed both project from the
    /// one current source. A rebuild advances the generation, so existing
    /// clients see that change too. Afterwards the cache is always present.
    pub(super) fn refresh_stale_shell_session_cache(&mut self) {
        let state = self.app.state();
        let cache_is_current = self
            .shell_session_cache
            .as_ref()
            .is_some_and(|cache| state.shell_projection_is_current(cache.revision));
        if !cache_is_current {
            self.rebuild_shell_session_cache();
            self.shell_session_generation.advance();
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
            let candidate = Self::snapshot_from_session(
                &self.app,
                &cache.session,
                &self.client_shell_boot_id,
                shell.projection_revision,
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
                    projection_revision: shell.projection_revision,
                    snapshot: candidate,
                },
            );
            if client_changed {
                changed = true;
                break;
            }
        }
        if changed {
            self.shell_session_generation.advance();
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
        let view = self.viewed_workspace_for_client(client_id)?;
        let pane_id = view.workspace.tree().focused();
        self.app
            .pane_runtime(pane_id)
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
                let modes = focused.and_then(|(runtime, _)| runtime.read().input_modes());
                let child_requests_mouse =
                    modes.is_some_and(shepr_vt::InputModes::mouse_tracking_enabled);
                // Pixels are for a client whose own host is exact, looking at a
                // pane whose child asked for 1016 and was told an extent, at
                // the grid the PTY has. The one rule is `pixel_mouse_eligible`.
                let pixels = presenting
                    && focused.zip(modes).is_some_and(|((runtime, _), modes)| {
                        self.presented_pane_grid(client_id, runtime)
                            .is_some_and(|grid| {
                                shepr_term::mouse::pixel_mouse_eligible(
                                    client.host_cell,
                                    modes.pixel_mouse(),
                                    grid,
                                )
                                .is_some()
                            })
                    });
                let capture = presenting && (shell.mouse_capture || child_requests_mouse);
                (client_id, HostMouseCapture::new(capture, pixels))
            })
            .collect::<Vec<_>>();

        for (client_id, mode) in requested {
            if let Some(client) = self.clients.get_mut(&client_id) {
                client.outbox.tell_mouse_capture(mode);
            }
        }
    }

    /// The grid at which `client_id` presents the pane `runtime` of the
    /// workspace it views, when that is the PTY's grid: its surface area is the
    /// area the workspace's PTYs were last laid out in (layout is a pure
    /// function of state and area). `None` when the areas differ: the client
    /// shows the pane at another grid, which is never eligible. Advisory: it
    /// picks the host's report mode, and the per-report decisions (the client's
    /// hit mapping, the server's admission) check the exact presented grid.
    fn presented_pane_grid(
        &self,
        client_id: ClientId,
        runtime: &shepr_mux::pane::PaneRuntime,
    ) -> Option<shepr_core::geometry::GridSize> {
        let view = self.viewed_workspace_for_client(client_id)?;
        let client = self.clients.get(&client_id)?;
        (view
            .workspace
            .spawn_geometry()
            .map(|geometry| geometry.area)
            == Some(client.terminal_size.rect()))
        .then(|| runtime.grid_size())
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
                                    && runtime.modify_other_keys_level()
                                        != shepr_vt::ModifyOtherKeysLevel::Off)
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
        for (_, view) in self.presented_views() {
            pane_ids.extend(view.workspace.tree().visible_pane_ids());
        }
        self.outputs.render().set_immediate_pty_sources(pane_ids);
    }

    pub(super) fn pty_sources_visible_to_any_render_target(
        &self,
        sources: &HashSet<shepr_core::layout::PaneId>,
    ) -> bool {
        if !self.has_app_client() {
            return false;
        }

        // A source that is no longer a pane is visible (its removal needs a
        // render); otherwise some presenting client's workspace must show one.
        // Each client's view is resolved once, not once per source.
        sources
            .iter()
            .any(|&pane_id| self.app.state().pane(pane_id).is_none())
            || self.presented_views().any(|(_, view)| {
                sources
                    .iter()
                    .any(|&pane_id| view.workspace.tree().shows(pane_id))
            })
    }

    /// What each attached client is owed this iteration, derived from its own
    /// settle point, location, baseline and slot plus the server-wide epoch.
    /// The checks run cheapest first: a client already due for a full pass
    /// never reaches `surface_deliverable`, which reads each visible pane's
    /// lock-free synchronized-output mirror.
    pub(super) fn render_plan(&mut self, render_signal_pending: bool) -> RenderPlan {
        self.settle_workspace_geometry_before_plan(render_signal_pending);
        // With no client attached nothing is drawn: settlement above already
        // laid out every workspace without a recorded area at the headless
        // geometry, so no render pass is owed for that.
        let mut plan = RenderPlan {
            full: Vec::new(),
            patch: Vec::new(),
        };
        for target in render_targets(&self.clients) {
            let Some(client) = self.clients.get(&target.client_id) else {
                continue;
            };
            let shell = client.shell_state();
            let full = !client.render_state.is_settled_at(self.view_epoch)
                || shell.projection_due(self.shell_session_generation)
                || (client.presents_surface()
                    && client.render_state.surface_debt()
                    && self.surface_deliverable(
                        target.client_id,
                        self.surface_target_for_client(target.client_id),
                    ));
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
    ///
    /// Reads the panes' lock-free mirror (`surface_held`), so planning never
    /// waits on a PTY reader. The mirror can lag a racing update by one
    /// mutation at worst; the surface render's own locked check still defers
    /// a frame drawn from a held pane.
    fn workspace_surface_held(&self, target: crate::ui::SurfaceTarget) -> bool {
        target.workspace(self.app.state()).is_some_and(|workspace| {
            self.runtimes_shown_by(workspace)
                .any(|runtime| runtime.read().surface_held())
        })
    }

    /// The workspace `id` views, resolved against the current state: its
    /// position and id, or `None` when its location names no live workspace.
    fn surface_target_for_client(&self, id: ClientId) -> Option<crate::ui::SurfaceTarget> {
        self.viewed_workspace_for_client(id)
            .map(|view| view.target())
    }

    /// Whether `id` can take a surface now: its slot is free and the
    /// workspace it views (`view`, resolved by the caller) is not held.
    fn surface_deliverable(&self, id: ClientId, view: Option<crate::ui::SurfaceTarget>) -> bool {
        let Some(client) = self.clients.get(&id) else {
            return false;
        };
        if !client.outbox.surface_slot_free() {
            return false;
        }
        let Some(view) = view else {
            return true;
        };
        !self.workspace_surface_held(view)
    }

    pub(super) fn render_pass(
        &mut self,
        plan: &RenderPlan,
        sources: &HashSet<shepr_core::layout::PaneId>,
    ) -> PassReport {
        self.render_pass_with_boundary(plan, sources, LiveSurfaceBoundary)
    }

    // Inject the framing boundary so failure recovery can be verified without
    // constructing a surface larger than the protocol's one GiB limit.
    pub(super) fn render_pass_with_boundary<B: SurfaceBoundary>(
        &mut self,
        plan: &RenderPlan,
        sources: &HashSet<shepr_core::layout::PaneId>,
        boundary: B,
    ) -> PassReport {
        let epoch = self.view_epoch;
        let mut report = PassReport::default();
        let mut full = plan.full.clone();
        if !sources.is_empty() {
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

        full.sort_unstable();
        full.dedup();
        self.render_full(&full, epoch, &mut report, boundary);
        if let Some(cache) = self.shell_session_cache.as_mut() {
            cache.timer_projections.clear();
        }
        debug!(?report, "rendered client surfaces");
        report
    }

    fn render_full<B: SurfaceBoundary>(
        &mut self,
        ids: &[ClientId],
        epoch: ViewEpoch,
        report: &mut PassReport,
        boundary: B,
    ) {
        // Each client's view is resolved once here and carried through the
        // pass, along with whether it can take a surface.
        let pass_clients = render_targets(&self.clients)
            .filter(|target| ids.contains(&target.client_id))
            .map(|target| {
                let view = self.surface_target_for_client(target.client_id);
                let deliverable = self.surface_deliverable(target.client_id, view);
                PassClient {
                    target,
                    view,
                    deliverable,
                }
            })
            .collect::<Vec<_>>();
        let mut shared = SharedSurfaces {
            boundary,
            remaining: HashMap::new(),
            rendered: HashMap::new(),
            surface_renders: 0,
            oversized_notices: Vec::new(),
        };
        for pass_client in &pass_clients {
            let Some(client) = self.clients.get(&pass_client.target.client_id) else {
                continue;
            };
            if !client.presents_surface() || !pass_client.deliverable {
                continue;
            }
            let key = pane_surface_render_key(pass_client.view, pass_client.target.area());
            *shared.remaining.entry(key).or_insert(0usize) += 1;
        }

        // Rebuild the shared session only when application state that feeds
        // it changed. `/proc`-derived fields are rechecked by the headless
        // loop's timer (`refresh_shell_projection_sources`), not here.
        if !pass_clients.is_empty() {
            self.refresh_stale_shell_session_cache();
        }
        for pass_client in pass_clients {
            let client_id = pass_client.target.client_id;
            let outcome = self.render_client_full(&pass_client, &mut shared);
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
        for OversizedNotice {
            client_id,
            claimed,
            max,
        } in shared.oversized_notices
        {
            let notice = ServerMessage::ClientShellError {
                kind: shepr_protocol::NoticeKind::LimitExceeded(
                    shepr_protocol::LimitExceeded::new(
                        shepr_protocol::Limit::new(
                            shepr_protocol::LimitKind::SurfaceMessageBytes,
                            max,
                        ),
                        claimed,
                    ),
                ),
            };
            self.send_to_client(client_id, &notice);
        }
    }
    fn render_client_full<B: SurfaceBoundary>(
        &mut self,
        pass_client: &PassClient,
        shared: &mut SharedSurfaces<B>,
    ) -> ClientPassOutcome {
        let client_id = pass_client.target.client_id;
        let area = pass_client.target.area();
        let deliverable = pass_client.deliverable;
        let shell_target = pass_client.view;
        let viewed = shell_target.and_then(|view| ViewedWorkspace::at(&self.app, view));
        let Some(client) = self.clients.get_mut(&client_id) else {
            return ClientPassOutcome::Closed;
        };
        // A projection is due when the shared session moved, or when this
        // client's own location did: a location change invalidates only
        // the projection of the client that moved.
        let needs_projection = {
            let shell = client.shell_state();
            shell.projection_due(self.shell_session_generation)
        };
        if needs_projection {
            let (location_generation, projection_revision) = {
                let shell = client.shell_state();
                (shell.location.generation(), shell.projection_revision)
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
                Self::snapshot_from_viewed_workspace(
                    &self.app,
                    &cache.session,
                    &self.client_shell_boot_id,
                    projection_revision,
                    viewed.as_ref(),
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
            let key = pane_surface_render_key(shell_target, area);
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
                    shared.boundary.render(&self.app, shell_target, area)
                })
            } else if let Some(result) = shared.rendered.get(&key) {
                result.clone()
            } else {
                shared.surface_renders += 1;
                let result = shared.boundary.render(&self.app, shell_target, area);
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
        let Some(crate::server::pane_surface::RenderedPaneSurface {
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

        let prepared = client.render_state.prepare_surface(
            shepr_protocol::PaneSurfaceFrame {
                boot_id: self.client_shell_boot_id.clone(),
                projection_revision: shell_projection_revision,
                // Placeholder: `prepare_surface` assigns the real revision
                // before any plan or message is built, and a surface it does
                // not accept is dropped unsent.
                surface_revision: shepr_protocol::SurfaceRevision::ZERO,
                frame,
                panes,
                splits,
            },
            pane_identities,
        );
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
        let serialized = match shared.boundary.encode(prepared.message()) {
            Ok(frame) => frame,
            Err(shepr_protocol::FramingError::LimitExceeded(error)) => {
                let claimed = error.actual;
                let max = error.limit.max();
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
                    shared.oversized_notices.push(OversizedNotice {
                        client_id,
                        claimed,
                        max,
                    });
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

impl HeadlessServer {
    /// Projects an already built `app.projection_input()` for one shell
    /// client.
    ///
    /// The session snapshot underneath is cached by the headless server and shared
    /// across clients. Borrowing it avoids cloning its source vectors before
    /// building the owned client snapshot; fields carried onto the wire still need
    /// their own owned values. Rendering re-projects it when the shared generation
    /// moves, this client's location generation moves, or its snapshot is
    /// missing. The shared generation advances after an application revision or
    /// when the cwd timer finds a changed projection; a location change invalidates
    /// only the client that moved.
    pub(in crate::server) fn snapshot_from_session(
        app: &app::App,
        snapshot: &crate::app::ProjectionInput,
        boot_id: &shepr_protocol::BootId,
        revision: shepr_protocol::ProjectionRevision,
        location: &crate::server::clients::ClientShellLocation,
    ) -> shepr_protocol::ClientShellSnapshot {
        let viewed = ViewedWorkspace::for_location(app, location);
        Self::snapshot_from_viewed_workspace(app, snapshot, boot_id, revision, viewed.as_ref())
    }

    fn snapshot_from_viewed_workspace(
        app: &app::App,
        snapshot: &crate::app::ProjectionInput,
        boot_id: &shepr_protocol::BootId,
        revision: shepr_protocol::ProjectionRevision,
        viewed: Option<&ViewedWorkspace<'_>>,
    ) -> shepr_protocol::ClientShellSnapshot {
        // A client with no live workspace has no focus; never use the bookmark.
        let focused_workspace_id = viewed.as_ref().map(|view| view.workspace.id());
        let focused_pane_id = viewed.as_ref().and_then(|view| {
            view.workspace
                .tree()
                .pane(view.workspace.tree().focused())
                .map(|record| {
                    shepr_protocol::PublicPaneId::new(&view.workspace.id(), record.number())
                })
        });
        // Snapshot entries are joined to live state by their public ids, never by
        // position: a snapshot that filtered or reordered entries would otherwise
        // hand one workspace's labels and branch to another.
        let workspaces = snapshot
            .workspaces
            .iter()
            .map(|workspace| {
                let workspace_id = &workspace.workspace_id;
                let state = app.state().workspace(workspace_id);
                let new_workspace_cwd = state.map(|_| {
                    shepr_protocol::RemotePath::from(
                        app.resolved_new_workspace_cwd(workspace_id).into_path_buf(),
                    )
                });
                shepr_protocol::ClientShellWorkspace {
                    workspace_id: *workspace_id,
                    new_workspace_cwd,
                    label: workspace.label.clone(),
                    branch: state
                        .and_then(shepr_mux::workspace::Workspace::branch)
                        .map(str::to_owned),
                    git_ahead_behind: state
                        .and_then(shepr_mux::workspace::Workspace::git_ahead_behind)
                        .map(|counts| (counts.ahead, counts.behind)),
                    agent_status: workspace.agent_status,
                    zoomed: state.is_some_and(|ws| ws.tree().zoomed()),
                }
            })
            .collect();
        let panes = snapshot
            .panes
            .iter()
            .map(|pane| {
                let right_click_passthrough = app
                    .state()
                    .resolve_pane(&pane.pane_id)
                    .is_some_and(|pane| pane.record().right_click_passthrough());
                shepr_protocol::ClientShellPane {
                    pane_id: pane.pane_id,
                    label: pane.label.clone(),
                    cwd: pane.cwd.clone(),
                    foreground_cwd: pane.foreground_cwd.clone(),
                    right_click_passthrough,
                }
            })
            .collect();
        let agents = snapshot
            .agents
            .iter()
            .map(|agent| shepr_protocol::ClientShellAgent {
                pane_id: agent.pane_id,
                agent: agent.agent,
                terminal_title: agent.terminal_title.clone(),
                terminal_title_stripped: agent.terminal_title_stripped.clone(),
                agent_status: agent.agent_status,
                state_change_seq: agent.state_change_seq,
            })
            .collect();

        shepr_protocol::ClientShellSnapshot {
            boot_id: boot_id.clone(),
            revision,
            restore_notice: app.restore_notice().cloned(),
            session_save_status: app.session_save_status(),
            focused_workspace_id,
            focused_pane_id,
            workspaces,
            panes,
            agents,
        }
    }
}
