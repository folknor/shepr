use super::*;

#[derive(Clone, Debug)]
pub(crate) struct ClientShellEndpoint {
    pub(crate) endpoint_id: ClientEndpointId,
    pub(crate) status: ClientEndpointStatus,
    pub(crate) snapshot: Option<Box<ClientShellSnapshot>>,
    /// Config bytes are stable for a server boot; cache their launch-time parse across snapshots.
    pub(crate) resolved_config: Option<CachedEndpointConfig>,
    /// Cache a failed wire value so reuse markers preserve its error without decoding again.
    pub(crate) resolved_config_error: Option<CachedEndpointConfigError>,
    /// Connection generation that produced `snapshot`. `None` is reserved for local tests.
    pub(crate) snapshot_generation: Option<u64>,
    pub(crate) agent_recency: HashMap<shepr_protocol::PublicPaneId, u64>,
}

#[derive(Clone)]
pub(crate) struct CachedEndpointConfig {
    pub(crate) wire: Vec<u8>,
    pub(crate) config: std::sync::Arc<shepr_config::ValidatedConfig>,
}

#[derive(Clone, Debug)]
pub(crate) struct CachedEndpointConfigError {
    pub(crate) wire: Vec<u8>,
    pub(crate) error: shepr_protocol::codec::CodecError,
}

#[derive(Clone, Debug)]
struct EndpointConfigurationError {
    endpoint_id: ClientEndpointId,
    cause: EndpointConfigurationCause,
}

#[derive(Clone, Debug)]
enum EndpointConfigurationCause {
    Decode(shepr_protocol::codec::CodecError),
    EndpointUnavailable,
    MissingConfiguration,
}

impl EndpointConfigurationError {
    fn decode(endpoint_id: &ClientEndpointId, error: shepr_protocol::codec::CodecError) -> Self {
        Self {
            endpoint_id: endpoint_id.clone(),
            cause: EndpointConfigurationCause::Decode(error),
        }
    }

    fn unavailable(endpoint_id: &ClientEndpointId) -> Self {
        Self {
            endpoint_id: endpoint_id.clone(),
            cause: EndpointConfigurationCause::EndpointUnavailable,
        }
    }

    fn missing(endpoint_id: &ClientEndpointId) -> Self {
        Self {
            endpoint_id: endpoint_id.clone(),
            cause: EndpointConfigurationCause::MissingConfiguration,
        }
    }
}

impl std::fmt::Display for EndpointConfigurationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.cause {
            EndpointConfigurationCause::Decode(error) => write!(formatter, "{error}"),
            EndpointConfigurationCause::EndpointUnavailable => {
                formatter.write_str("endpoint is no longer available")
            }
            EndpointConfigurationCause::MissingConfiguration => formatter
                .write_str("endpoint snapshot omitted configuration without a cached value"),
        }
    }
}

// Display includes the decode cause, so leave the source chain empty to avoid repeating it.
impl std::error::Error for EndpointConfigurationError {}

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
    Workspace(shepr_protocol::WorkspaceId),
    Pane(shepr_protocol::PublicPaneId),
}

impl ClientShellState {
    /// Sets the configured machines, once at launch: Local first, then one endpoint per
    /// `[[machines]]` entry, each Connecting with no snapshot. The set never changes
    /// while the client runs.
    pub fn set_machines(&mut self, machines: &[shepr_config::MachineConfig]) {
        let mut next = Vec::with_capacity(machines.len().saturating_add(1));
        let local = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id.is_local())
            .cloned()
            .unwrap_or_else(local_endpoint);
        next.push(local);
        for machine in machines {
            next.push(ClientShellEndpoint {
                endpoint_id: ClientEndpointId::Ssh(machine.label.clone()),
                status: ClientEndpointStatus::Connecting,
                snapshot: None,
                resolved_config: None,
                resolved_config_error: None,
                snapshot_generation: None,
                agent_recency: HashMap::new(),
            });
        }
        self.endpoints = next;
    }

    pub fn set_endpoint_status(
        &mut self,
        endpoint_id: &ClientEndpointId,
        status: ClientEndpointStatus,
    ) {
        let configuration_error = self.endpoints.iter().any(|endpoint| {
            &endpoint.endpoint_id == endpoint_id && endpoint.resolved_config_error.is_some()
        });
        let status = if status == ClientEndpointStatus::Online && configuration_error {
            ClientEndpointStatus::Attention
        } else {
            status
        };
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
            if let Some(error) = endpoint.resolved_config_error.as_ref() {
                let error =
                    EndpointConfigurationError::decode(&endpoint.endpoint_id, error.error.clone());
                let message = self.endpoint_configuration_message(&error);
                self.set_endpoint_error(message, self.now);
            }
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
                let message = self.endpoint_configuration_message(&error);
                self.set_endpoint_error(message, self.now);
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

    pub(crate) fn endpoint_boot_id(
        &self,
        endpoint_id: &ClientEndpointId,
    ) -> Option<&shepr_protocol::BootId> {
        self.endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)?
            .snapshot
            .as_deref()
            .map(|snapshot| &snapshot.boot_id)
    }

    pub(crate) fn endpoint_snapshot_matches(
        &self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
        boot_id: &shepr_protocol::BootId,
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
                        snapshot.boot_id == *boot_id && snapshot.revision == revision
                    })
            })
    }

    pub(crate) fn endpoint_snapshot_identity(
        &self,
        endpoint_id: &ClientEndpointId,
        generation: u64,
    ) -> Option<(&shepr_protocol::BootId, u64)> {
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
            .map(|snapshot| (&snapshot.boot_id, snapshot.revision.get()))
    }

    /// A terminal normally starts focused. `None` means this host cannot report focus events,
    /// not that the endpoint has no viewer; activation therefore sends an explicit true baseline.
    pub(crate) fn host_focus_baseline(&self) -> bool {
        self.outer_focused.unwrap_or(true)
    }

    pub(crate) fn endpoint_label<'a>(&self, endpoint_id: &'a ClientEndpointId) -> &'a str {
        endpoint_id.display_label()
    }

    pub(crate) fn active_endpoint_label(&self) -> &str {
        self.active_endpoint_id.display_label()
    }

    pub fn endpoint_is_active(&self, endpoint_id: &ClientEndpointId) -> bool {
        &self.active_endpoint_id == endpoint_id
    }

    pub(crate) fn multi_endpoint_active(&self) -> bool {
        self.endpoints.len() > 1
    }

    pub(super) fn focused_tab_count(&self) -> usize {
        self.snapshot
            .as_deref()
            .map_or(0, |snapshot| workspace_tab_count(snapshot, None))
    }

    /// The pane surface size `endpoint_id` will lay out once its cached projection is active and
    /// focused on `focus` (without one, on the workspace its snapshot focuses). A handoff sizes
    /// its surface requests by this, since the projection it commits is not the active one yet.
    /// Shell chrome is the client's own, so projections lay out differently only through the
    /// focused workspace's tab count (the tab bar hides for a single tab with
    /// `hide_tab_bar_when_single_tab`). An endpoint with no cached snapshot gets the active
    /// layout's size.
    pub(crate) fn endpoint_surface_size(
        &self,
        endpoint_id: &ClientEndpointId,
        focus: Option<&ClientEndpointFocusTarget>,
        cols: u16,
        rows: u16,
    ) -> ClientSurfaceSize {
        let tab_count = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .and_then(|endpoint| endpoint.snapshot.as_deref())
            .map_or_else(
                || self.focused_tab_count(),
                |snapshot| workspace_tab_count(snapshot, focus),
            );
        self.surface_size_with_tab_count(cols, rows, tab_count)
    }

    /// Like `endpoint_surface_size` for an endpoint whose cached projection is `snapshot`,
    /// a snapshot that has arrived but is not cached yet.
    pub(crate) fn snapshot_surface_size(
        &self,
        snapshot: &ClientShellSnapshot,
        focus: Option<&ClientEndpointFocusTarget>,
        cols: u16,
        rows: u16,
    ) -> ClientSurfaceSize {
        self.surface_size_with_tab_count(cols, rows, workspace_tab_count(snapshot, focus))
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
        // Decode every endpoint's config as it arrives, active or not, so a bad
        // one is reported at once. The Result below is only a duplicate return
        // channel: `cache_endpoint_config` stores success or failure on the
        // endpoint, updates its status and diagnostic, and logs decode failures
        // with endpoint identity. Active projection reads that same cache. An
        // empty value reuses the connection's previous config, or is reported
        // once when the connection has no cached value.
        let endpoint = &self.endpoints[index];
        if !snapshot.resolved_config.is_empty()
            || (endpoint.resolved_config.is_none() && endpoint.resolved_config_error.is_none())
        {
            self.cache_endpoint_config(endpoint_id, &snapshot.resolved_config)
                .ok();
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
                recency.insert(agent.pane_id.clone(), next_recency);
            }
        }
        let live_agent_ids = snapshot
            .agents
            .iter()
            .map(|agent| &agent.pane_id)
            .collect::<std::collections::HashSet<_>>();
        recency.retain(|pane_id, _| live_agent_ids.contains(pane_id));
        let endpoint = &mut self.endpoints[index];
        endpoint.agent_recency = recency;
        endpoint.snapshot_generation = generation;
        endpoint.snapshot = Some(snapshot);
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
                    let message = self.endpoint_configuration_message(&error);
                    self.set_endpoint_error(message, self.now);
                }
            }
        }
    }

    fn resolve_snapshot_config(
        &mut self,
        endpoint_id: &ClientEndpointId,
        snapshot: &ClientShellSnapshot,
    ) -> Result<std::sync::Arc<shepr_config::ValidatedConfig>, EndpointConfigurationError> {
        let endpoint = self
            .endpoints
            .iter()
            .find(|endpoint| &endpoint.endpoint_id == endpoint_id)
            .ok_or_else(|| EndpointConfigurationError::unavailable(endpoint_id))?;
        let cached_error = endpoint
            .resolved_config_error
            .as_ref()
            // A zero-length config is the connection-local reuse marker.
            .filter(|cached| {
                snapshot.resolved_config.is_empty() || cached.wire == snapshot.resolved_config
            })
            .map(|cached| cached.error.clone());
        if let Some(error) = cached_error {
            return Err(EndpointConfigurationError::decode(endpoint_id, error));
        }
        let cached_config = endpoint
            .resolved_config
            .as_ref()
            .filter(|cached| {
                snapshot.resolved_config.is_empty() || cached.wire == snapshot.resolved_config
            })
            .map(|cached| std::sync::Arc::clone(&cached.config));
        if let Some(config) = cached_config {
            return Ok(config);
        }

        if snapshot.resolved_config.is_empty() {
            return Err(EndpointConfigurationError::missing(endpoint_id));
        }
        self.cache_endpoint_config(endpoint_id, &snapshot.resolved_config)
    }

    fn cache_endpoint_config(
        &mut self,
        endpoint_id: &ClientEndpointId,
        wire: &[u8],
    ) -> Result<std::sync::Arc<shepr_config::ValidatedConfig>, EndpointConfigurationError> {
        let Some(index) = self
            .endpoints
            .iter()
            .position(|endpoint| &endpoint.endpoint_id == endpoint_id)
        else {
            return Err(EndpointConfigurationError::unavailable(endpoint_id));
        };
        if let Some(cached) = self.endpoints[index]
            .resolved_config_error
            .as_ref()
            .filter(|cached| cached.wire == wire)
        {
            return Err(EndpointConfigurationError::decode(
                endpoint_id,
                cached.error.clone(),
            ));
        }
        if let Some(config) = self.endpoints[index]
            .resolved_config
            .as_ref()
            .filter(|cached| cached.wire == wire)
            .map(|cached| std::sync::Arc::clone(&cached.config))
        {
            if self.endpoints[index].resolved_config_error.take().is_some() {
                self.set_endpoint_status(endpoint_id, ClientEndpointStatus::Online);
            }
            return Ok(config);
        }

        match shepr_protocol::codec::from_slice_exact::<shepr_config::ValidatedConfig>(wire) {
            Ok(config) => {
                let config = std::sync::Arc::new(config);
                let had_error = {
                    let endpoint = &mut self.endpoints[index];
                    let had_error = endpoint.resolved_config_error.take().is_some();
                    endpoint.resolved_config = Some(CachedEndpointConfig {
                        wire: wire.to_vec(),
                        config: std::sync::Arc::clone(&config),
                    });
                    had_error
                };
                if had_error {
                    self.set_endpoint_status(endpoint_id, ClientEndpointStatus::Online);
                }
                Ok(config)
            }
            Err(error) => {
                let error_message = error.to_string();
                {
                    let endpoint = &mut self.endpoints[index];
                    endpoint.resolved_config_error = Some(CachedEndpointConfigError {
                        wire: wire.to_vec(),
                        error: error.clone(),
                    });
                }
                self.set_endpoint_status(endpoint_id, ClientEndpointStatus::Attention);
                self.set_machine_error(
                    endpoint_id,
                    &format!("invalid endpoint configuration: {error_message}"),
                );
                tracing::warn!(
                    endpoint = %endpoint_id.storage_key(),
                    error = %error_message,
                    "endpoint configuration could not be decoded"
                );
                Err(EndpointConfigurationError::decode(endpoint_id, error))
            }
        }
    }

    fn endpoint_configuration_message(&self, error: &EndpointConfigurationError) -> String {
        let label = self.endpoint_label(&error.endpoint_id);
        format!("{label}: invalid endpoint configuration: {error}")
    }
}

/// The tab count of the workspace `focus` names in `snapshot`, or of the snapshot's focused
/// workspace when there is no focus target or its pane is not in the snapshot.
fn workspace_tab_count(
    snapshot: &ClientShellSnapshot,
    focus: Option<&ClientEndpointFocusTarget>,
) -> usize {
    let workspace_id = match focus {
        Some(ClientEndpointFocusTarget::Workspace(workspace_id)) => Some(workspace_id.as_str()),
        Some(ClientEndpointFocusTarget::Pane(pane_id)) => snapshot
            .panes
            .iter()
            .find(|pane| &pane.pane_id == pane_id)
            .map(|pane| pane.workspace_id.as_str()),
        None => None,
    }
    .or(snapshot.focused_workspace_id.as_deref());
    snapshot
        .tabs
        .iter()
        .filter(|tab| Some(tab.workspace_id.as_str()) == workspace_id)
        .count()
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
        status: ClientEndpointStatus::Online,
        snapshot: None,
        resolved_config: None,
        resolved_config_error: None,
        snapshot_generation: None,
        agent_recency: HashMap::new(),
    }
}

#[cfg(test)]
impl ClientShellState {
    pub fn set_snapshot(&mut self, snapshot: Box<ClientShellSnapshot>) {
        let endpoint_id = self.active_endpoint_id.clone();
        self.set_endpoint_snapshot(&endpoint_id, snapshot);
    }

    pub(crate) fn cache_endpoint_snapshot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        snapshot: Box<ClientShellSnapshot>,
    ) {
        self.cache_endpoint_snapshot_at_generation(endpoint_id, None, snapshot);
    }

    pub fn set_endpoint_snapshot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        snapshot: Box<ClientShellSnapshot>,
    ) {
        self.cache_endpoint_snapshot(endpoint_id, snapshot);
        self.apply_cached_endpoint_snapshot(endpoint_id);
    }
}
