use super::*;

#[derive(Clone, Debug)]
pub(crate) struct ClientShellEndpoint {
    pub(crate) endpoint_id: ClientEndpointId,
    pub(crate) label: String,
    pub(crate) status: ClientEndpointStatus,
    pub(crate) snapshot: Option<Box<ClientShellSnapshot>>,
    /// Config bytes are stable for a server boot; cache their launch-time parse across snapshots.
    pub(crate) resolved_config: Option<CachedEndpointConfig>,
    /// Connection generation that produced `snapshot`. `None` is reserved for local tests.
    pub(crate) snapshot_generation: Option<u64>,
    pub(crate) agent_recency: HashMap<String, u64>,
}

#[derive(Clone)]
pub(crate) struct CachedEndpointConfig {
    pub(crate) wire: Vec<u8>,
    pub(crate) config: std::sync::Arc<shepr_config::ValidatedConfig>,
}

impl std::fmt::Debug for CachedEndpointConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CachedEndpointConfig")
            .field("wire_bytes", &self.wire.len())
            .finish_non_exhaustive()
    }
}

pub(super) struct MachineHit {
    pub(super) rect: Rect,
    pub(super) status_badge: Rect,
    pub(super) collapse_toggle: Rect,
    pub(super) endpoint_id: ClientEndpointId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientEndpointFocusTarget {
    Workspace(String),
    Pane(shepr_protocol::PublicPaneId),
}

impl ClientShellState {
    pub fn set_endpoint_catalog(&mut self, profiles: &[SavedSshEndpoint]) {
        let mut next = Vec::with_capacity(profiles.len().saturating_add(1));
        let local = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id.is_local())
            .cloned()
            .unwrap_or_else(local_endpoint);
        next.push(local);
        for profile in profiles {
            let endpoint_id = ClientEndpointId::Ssh(profile.id.clone());
            let previous = self
                .endpoints
                .iter()
                .find(|endpoint| endpoint.endpoint_id == endpoint_id);
            next.push(ClientShellEndpoint {
                endpoint_id,
                label: profile.label.clone(),
                status: previous
                    .map_or(ClientEndpointStatus::Connecting, |endpoint| endpoint.status),
                snapshot: previous.and_then(|endpoint| endpoint.snapshot.clone()),
                resolved_config: previous.and_then(|endpoint| endpoint.resolved_config.clone()),
                snapshot_generation: previous.and_then(|endpoint| endpoint.snapshot_generation),
                agent_recency: previous
                    .map(|endpoint| endpoint.agent_recency.clone())
                    .unwrap_or_default(),
            });
        }

        if !next
            .iter()
            .any(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)
        {
            self.select_unavailable_local();
        }
        self.collapsed_endpoints.retain(|endpoint_id| {
            next.iter()
                .any(|endpoint| &endpoint.endpoint_id == endpoint_id)
        });
        self.endpoints = next;
    }

    pub(crate) fn select_unavailable_local(&mut self) {
        self.reset_endpoint_projection();
        self.active_endpoint_id = ClientEndpointId::Local;
        self.mode = ClientShellMode::Terminal;
        self.snapshot = None;
        self.active_resolved_config = None;
        self.graphics_scope = "local:unavailable".to_owned();
    }

    pub fn set_endpoint_status(
        &mut self,
        endpoint_id: &ClientEndpointId,
        status: ClientEndpointStatus,
    ) {
        if status == ClientEndpointStatus::Online {
            self.clear_machine_diagnostic(endpoint_id);
        }
        if endpoint_id == &self.active_endpoint_id && status != ClientEndpointStatus::Online {
            self.pending_workspace_highlight = None;
        }
        if let Some(endpoint) = self
            .endpoints
            .iter_mut()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        {
            endpoint.status = status;
        }
    }

    pub(crate) fn mark_endpoint_disconnected(&mut self, endpoint_id: &ClientEndpointId) {
        self.set_endpoint_status(endpoint_id, ClientEndpointStatus::Reconnecting);
        if endpoint_id == &self.active_endpoint_id {
            let pending = self.pending_requests.keys().cloned().collect::<Vec<_>>();
            for request_id in pending {
                self.cancel_endpoint_request(&request_id);
            }
            self.pane_scroll_in_flight.clear();
            self.pane_scroll_queued.clear();
        }
    }

    pub(crate) fn endpoint_projection_available(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.endpoints.iter().any(|endpoint| {
            &endpoint.endpoint_id == endpoint_id
                && endpoint.status == ClientEndpointStatus::Online
                && endpoint.snapshot.is_some()
        })
    }

    pub(crate) fn activate_endpoint_projection(&mut self, endpoint_id: &ClientEndpointId) -> bool {
        let Some(endpoint) = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        else {
            return false;
        };
        if endpoint.status != ClientEndpointStatus::Online {
            return false;
        }
        let Some(snapshot) = endpoint.snapshot.clone() else {
            return false;
        };
        let generation = endpoint.snapshot_generation;
        // Resolve the endpoint's config before changing the active id or clearing the
        // previous pane surface. A malformed config must not make the old projection
        // appear to belong to this endpoint.
        let snapshot_config = match self.resolve_snapshot_config(endpoint_id, &snapshot) {
            Ok(config) => config,
            Err(error) => {
                self.set_endpoint_error(format!("invalid endpoint configuration: {error}"));
                return false;
            }
        };
        let pending_agent_reveal = self
            .pending_agent_reveal
            .take_if(|(target_endpoint, _)| target_endpoint == endpoint_id);
        let agent_body_height = self.hits.agent_body.height;
        let switching_endpoint = endpoint_id != &self.active_endpoint_id;
        let agent_scroll = self.agent_scroll;
        if switching_endpoint {
            self.active_endpoint_id = endpoint_id.clone();
            self.pane_surface = None;
            self.pending_pane_surface = None;
        }
        self.apply_active_snapshot(snapshot, generation, &snapshot_config);
        if switching_endpoint {
            // The aggregate agent list belongs to the client, not one endpoint.
            self.agent_scroll = agent_scroll;
        }
        if let Some((_, pane_id)) = pending_agent_reveal {
            self.reveal_endpoint_agent(endpoint_id, &pane_id, agent_body_height);
        }
        true
    }

    pub(crate) fn endpoint_status(
        &self,
        endpoint_id: &ClientEndpointId,
    ) -> Option<ClientEndpointStatus> {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .map(|endpoint| endpoint.status)
    }

    pub(crate) fn endpoint_has_snapshot(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .is_some_and(|endpoint| endpoint.snapshot.is_some())
    }

    pub(crate) fn endpoint_is_online(&self, endpoint_id: &ClientEndpointId) -> bool {
        self.endpoint_status(endpoint_id) == Some(ClientEndpointStatus::Online)
            && self.endpoint_has_snapshot(endpoint_id)
    }

    pub(crate) fn endpoint_boot_id(&self, endpoint_id: &ClientEndpointId) -> Option<&str> {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)?
            .snapshot
            .as_deref()
            .map(|snapshot| snapshot.boot_id.as_str())
    }

    pub(crate) fn endpoint_snapshot_matches(
        &self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        boot_id: &str,
        revision: u64,
    ) -> bool {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .is_some_and(|endpoint| {
                endpoint
                    .snapshot_generation
                    .is_none_or(|snapshot_generation| snapshot_generation == generation)
                    && endpoint.snapshot.as_deref().is_some_and(|snapshot| {
                        snapshot.boot_id == boot_id && snapshot.revision == revision
                    })
            })
    }

    pub(crate) fn endpoint_snapshot_identity(
        &self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
    ) -> Option<(&str, u64)> {
        let endpoint = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)?;
        if endpoint
            .snapshot_generation
            .is_some_and(|snapshot_generation| snapshot_generation != generation)
        {
            return None;
        }
        endpoint
            .snapshot
            .as_deref()
            .map(|snapshot| (snapshot.boot_id.as_str(), snapshot.revision.get()))
    }

    /// A terminal normally starts focused. `None` means this host cannot report focus events,
    /// not that the endpoint has no viewer; activation therefore sends an explicit true baseline.
    pub(crate) fn host_focus_baseline(&self) -> bool {
        self.outer_focused.unwrap_or(true)
    }

    pub(crate) fn endpoint_label(&self, endpoint_id: &ClientEndpointId) -> &str {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .map_or("Unknown endpoint", |endpoint| endpoint.label.as_str())
    }

    pub(crate) fn active_endpoint_label(&self) -> &str {
        self.endpoint_label(&self.active_endpoint_id)
    }

    pub fn endpoint_is_active(&self, endpoint_id: &ClientEndpointId) -> bool {
        &self.active_endpoint_id == endpoint_id
    }

    pub(crate) fn multi_endpoint_active(&self) -> bool {
        self.endpoints.len() > 1
    }

    #[cfg(test)]
    pub fn set_snapshot(&mut self, snapshot: Box<ClientShellSnapshot>) {
        let endpoint_id = self.active_endpoint_id.clone();
        self.set_endpoint_snapshot(&endpoint_id, snapshot);
    }

    pub(super) fn focused_tab_count(&self) -> usize {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return 0;
        };
        snapshot
            .tabs
            .iter()
            .filter(|tab| {
                Some(tab.workspace_id.as_str()) == snapshot.focused_workspace_id.as_deref()
            })
            .count()
    }

    #[cfg(test)]
    pub(crate) fn cache_endpoint_snapshot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        snapshot: Box<ClientShellSnapshot>,
    ) {
        self.cache_endpoint_snapshot_at_generation(endpoint_id, None, snapshot);
    }

    pub(crate) fn cache_endpoint_snapshot_for_generation(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        snapshot: Box<ClientShellSnapshot>,
    ) {
        self.cache_endpoint_snapshot_at_generation(endpoint_id, Some(generation), snapshot);
    }

    fn cache_endpoint_snapshot_at_generation(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: Option<u64>,
        snapshot: Box<ClientShellSnapshot>,
    ) {
        let Some(index) = self
            .endpoints
            .iter()
            .position(|endpoint| &endpoint.endpoint_id == endpoint_id)
        else {
            return;
        };
        if self.endpoints[index].snapshot_generation == generation
            && self.endpoints[index]
                .snapshot
                .as_deref()
                .is_some_and(|previous| {
                    previous.boot_id == snapshot.boot_id && previous.revision > snapshot.revision
                })
        {
            return;
        }
        let previous = self.endpoints[index].snapshot.as_deref();
        let mut next_recency = self
            .endpoints
            .iter()
            .flat_map(|endpoint| endpoint.agent_recency.values())
            .copied()
            .max()
            .unwrap_or_default();
        let mut agents = snapshot.agents.iter().collect::<Vec<_>>();
        agents.sort_by_key(|agent| agent.state_change_seq);
        let mut recency = self.endpoints[index].agent_recency.clone();
        for agent in agents {
            let changed = previous
                .and_then(|snapshot| {
                    snapshot
                        .agents
                        .iter()
                        .find(|previous| previous.pane_id == agent.pane_id)
                })
                .is_none_or(|previous| previous.state_change_seq != agent.state_change_seq);
            if changed {
                next_recency = next_recency.saturating_add(1);
                recency.insert(agent.pane_id.to_string(), next_recency);
            }
        }
        let live_agent_ids = snapshot
            .agents
            .iter()
            .map(|agent| agent.pane_id.to_string())
            .collect::<std::collections::HashSet<_>>();
        recency.retain(|pane_id, _| live_agent_ids.contains(pane_id));
        let endpoint = &mut self.endpoints[index];
        if !snapshot.resolved_config.is_empty() {
            endpoint.resolved_config = shepr_protocol::codec::from_slice_exact::<
                shepr_config::ValidatedConfig,
            >(&snapshot.resolved_config)
            .ok()
            .map(|config| CachedEndpointConfig {
                wire: snapshot.resolved_config.clone(),
                config: std::sync::Arc::new(config),
            });
        }
        endpoint.agent_recency = recency;
        endpoint.snapshot_generation = generation;
        endpoint.snapshot = Some(snapshot);
    }

    #[cfg(test)]
    pub fn set_endpoint_snapshot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        snapshot: Box<ClientShellSnapshot>,
    ) {
        self.cache_endpoint_snapshot(endpoint_id, snapshot);
        self.apply_cached_endpoint_snapshot(endpoint_id);
    }

    pub fn set_endpoint_snapshot_for_generation(
        &mut self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        snapshot: Box<ClientShellSnapshot>,
    ) {
        self.cache_endpoint_snapshot_for_generation(endpoint_id, generation, snapshot);
        self.apply_cached_endpoint_snapshot(endpoint_id);
    }

    fn apply_cached_endpoint_snapshot(&mut self, endpoint_id: &ClientEndpointId) {
        let Some((snapshot, generation)) = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .and_then(|endpoint| {
                endpoint
                    .snapshot
                    .clone()
                    .map(|snapshot| (snapshot, endpoint.snapshot_generation))
            })
        else {
            return;
        };
        if endpoint_id == &self.active_endpoint_id {
            match self.resolve_snapshot_config(endpoint_id, &snapshot) {
                Ok(config) => self.apply_active_snapshot(snapshot, generation, &config),
                Err(error) => {
                    self.set_endpoint_error(format!("invalid endpoint configuration: {error}"));
                }
            }
        }
    }

    fn resolve_snapshot_config(
        &mut self,
        endpoint_id: &ClientEndpointId,
        snapshot: &ClientShellSnapshot,
    ) -> Result<std::sync::Arc<shepr_config::ValidatedConfig>, String> {
        let cached_config = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .and_then(|endpoint| endpoint.resolved_config.as_ref())
            // A zero-length config is the connection-local reuse marker.
            .filter(|cached| {
                snapshot.resolved_config.is_empty() || cached.wire == snapshot.resolved_config
            })
            .map(|cached| std::sync::Arc::clone(&cached.config));
        if let Some(config) = cached_config {
            return Ok(config);
        }

        let config = shepr_protocol::codec::from_slice_exact::<shepr_config::ValidatedConfig>(
            &snapshot.resolved_config,
        )
        .map_err(|error| error.to_string())?;
        let config = std::sync::Arc::new(config);
        if let Some(endpoint) = self
            .endpoints
            .iter_mut()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
        {
            endpoint.resolved_config = Some(CachedEndpointConfig {
                wire: snapshot.resolved_config.clone(),
                config: std::sync::Arc::clone(&config),
            });
        }
        Ok(config)
    }
}

pub(super) fn endpoint_status_presentation(
    status: ClientEndpointStatus,
    palette: &Palette,
) -> (&'static str, &'static str, ratatui::style::Color) {
    match status {
        ClientEndpointStatus::Connecting => ("◐", "connecting", palette.yellow),
        ClientEndpointStatus::Online => ("●", "online", palette.green),
        ClientEndpointStatus::Reconnecting => ("◐", "reconnecting", palette.yellow),
        ClientEndpointStatus::Attention => ("!", "attention", palette.red),
    }
}

pub(super) fn local_endpoint() -> ClientShellEndpoint {
    ClientShellEndpoint {
        endpoint_id: ClientEndpointId::Local,
        label: "Local".into(),
        status: ClientEndpointStatus::Online,
        snapshot: None,
        resolved_config: None,
        snapshot_generation: None,
        agent_recency: HashMap::new(),
    }
}
