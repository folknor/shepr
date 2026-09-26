use std::collections::HashMap;

use crate::protocol::{
    ClientKeyCode, ClientKeyKind, ClientMouseButton, ClientMouseKind, ClientPaneInputEvent,
    RenderEncoding,
};
use crate::server::client_transport::ClientWriter;
use crate::server::render_stream::ClientRenderState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClientConnectionMode {
    ClientShell,
    TerminalPending,
    TerminalAttach { terminal_id: String },
}

pub(crate) type RenderTarget = (
    u64,
    (u16, u16),
    crate::terminal_cell_size::HostCellSize,
    bool,
    ClientConnectionMode,
);

/// A held press, keyed by what the client reports: the key code (a Linux
/// terminal reports no physical key identity) or the mouse button.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ClientShellPressId {
    Key(ClientKeyCode),
    Mouse(ClientMouseButton),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClientShellHeldInput {
    pub(crate) target: String,
    pub(crate) release: ClientPaneInputEvent,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DeferredRender {
    #[default]
    None,
    Full,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClientShellLocation {
    pub(crate) focused_workspace_id: Option<String>,
    pub(crate) active_tab_ids: HashMap<String, String>,
}

pub(crate) struct ClientShellTopology {
    pub(crate) focused_workspace_id: Option<String>,
    pub(crate) fallback_workspace_id: Option<String>,
    pub(crate) active_tab_ids: HashMap<String, String>,
    pub(crate) tab_workspace_ids: HashMap<String, String>,
}

impl ClientShellLocation {
    pub(crate) fn from_snapshot(snapshot: &crate::protocol::ClientShellSnapshot) -> Self {
        Self {
            focused_workspace_id: snapshot.focused_workspace_id.clone(),
            active_tab_ids: snapshot
                .workspaces
                .iter()
                .map(|workspace| {
                    (
                        workspace.workspace_id.clone(),
                        workspace.active_tab_id.clone(),
                    )
                })
                .collect(),
        }
    }

    pub(crate) fn focused_tab_id(&self) -> Option<&str> {
        self.focused_workspace_id
            .as_deref()
            .and_then(|workspace_id| self.active_tab_ids.get(workspace_id))
            .map(String::as_str)
    }

    pub(crate) fn focus_workspace(&mut self, workspace_id: String) {
        self.focused_workspace_id = Some(workspace_id);
    }

    pub(crate) fn focus_tab(&mut self, workspace_id: String, tab_id: String) {
        self.focused_workspace_id = Some(workspace_id.clone());
        self.active_tab_ids.insert(workspace_id, tab_id);
    }

    pub(crate) fn reconcile(&mut self, topology: &ClientShellTopology) {
        self.active_tab_ids.retain(|workspace_id, tab_id| {
            topology.active_tab_ids.contains_key(workspace_id)
                && topology.tab_workspace_ids.get(tab_id) == Some(workspace_id)
        });
        for (workspace_id, tab_id) in &topology.active_tab_ids {
            self.active_tab_ids
                .entry(workspace_id.clone())
                .or_insert_with(|| tab_id.clone());
        }
        if self
            .focused_workspace_id
            .as_ref()
            .is_none_or(|workspace_id| !topology.active_tab_ids.contains_key(workspace_id))
        {
            self.focused_workspace_id = topology
                .focused_workspace_id
                .clone()
                .or_else(|| topology.fallback_workspace_id.clone());
        }
    }
}

/// A connected client tracked by the server.
pub(crate) struct ClientConnection {
    /// Whether this connection owns the Shepr shell or one direct terminal stream.
    pub(crate) mode: ClientConnectionMode,
    /// The client's terminal size after clamping.
    pub(crate) terminal_size: (u16, u16),
    /// Pixel size of one client terminal cell.
    pub(crate) cell_size: crate::terminal_cell_size::HostCellSize,
    /// Monotonic activity stamp used to choose the fallback foreground client.
    pub(crate) last_activity: u64,
    /// Render baseline for the negotiated client encoding.
    pub(crate) render_state: ClientRenderState,
    /// Whether this frontend preserves exact SGR pixel reports.
    pub(crate) pixel_mouse: bool,
    /// Last host terminal default colors reported by this client.
    pub(crate) host_terminal_theme: crate::terminal_theme::TerminalTheme,
    /// Last host light/dark appearance reported by this client.
    pub(crate) host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
    /// Whether appearance came from an explicit host color-scheme report.
    pub(crate) host_terminal_appearance_explicit: bool,
    /// Last reported focus state for this client's outer terminal.
    pub(crate) outer_terminal_focus: Option<bool>,
    /// Last focused-pane report-all demand sent to a client-owned shell.
    pub(crate) host_keyboard_report_all_active: Option<bool>,
    /// Whether an ordinary render was skipped because the render channel was full.
    pub(crate) render_pending: bool,
    /// Whether the client has been told that its current frame is too large to
    /// send. Set on the first oversized frame, cleared once a frame goes out, so
    /// a client whose frames keep failing is warned once rather than per render.
    pub(crate) oversized_frame_reported: bool,
    /// Whether a direct terminal-attach client has been told that its input
    /// is being dropped because the pane stopped reading. Set on the first
    /// drop, cleared by the next input that reaches the pane, so a user typing
    /// into a wedged pane gets one notice rather than one per keystroke.
    pub(crate) attach_input_drop_reported: bool,
    /// Whether this connection receives pane surfaces and may affect presentation state.
    pub(crate) shell_surface_active: bool,
    /// Whether this shell wants host mouse capture without pane demand.
    pub(crate) shell_mouse_capture: bool,
    /// Last host mouse capture mode sent to this client.
    pub(crate) host_mouse_capture_active: Option<bool>,
    /// Last SGR pixel provenance mode sent to this client.
    pub(crate) host_sgr_pixels_active: Option<bool>,
    /// Last keyboard protocol state sent to a directly attached terminal client.
    pub(crate) host_keyboard_protocol_active: Option<(u16, u8)>,
    /// Presses forwarded by this shell that need release on abrupt teardown.
    shell_held_inputs: HashMap<ClientShellPressId, ClientShellHeldInput>,
    /// Connection-local workspace and tab projection for a client-owned shell.
    pub(crate) shell_location: Option<ClientShellLocation>,
    /// Last coherent shell replacement sent to this client.
    pub(crate) shell_snapshot: Option<crate::protocol::ClientShellSnapshot>,
    /// Monotonic shell replacement revision for this connection.
    pub(crate) shell_projection_revision: u64,
    /// Whether this shell is waiting for one ordered endpoint command response.
    pub(crate) shell_endpoint_command_in_flight: bool,
    /// Channels for sending framed ServerMessage data to the client writer thread.
    ///
    /// Always `Some` in production: every accepted connection brings a writer,
    /// and a detach removes the client outright rather than keeping a
    /// writer-less entry. `None` exists for test fixtures that exercise
    /// server state without a transport; the `writer.is_none()` checks in the
    /// server serve those. Making the field non-optional would mean giving
    /// every such fixture a channel pair.
    pub(crate) writer: Option<ClientWriter>,
}

impl ClientConnection {
    #[cfg(test)]
    pub(crate) fn new(
        terminal_size: (u16, u16),
        cell_size: crate::terminal_cell_size::HostCellSize,
        last_activity: u64,
        render_encoding: RenderEncoding,
        writer: Option<ClientWriter>,
    ) -> Self {
        Self::new_with_mode(
            ClientConnectionMode::ClientShell,
            terminal_size,
            cell_size,
            last_activity,
            render_encoding,
            writer,
        )
    }

    pub(crate) fn new_with_mode(
        mode: ClientConnectionMode,
        terminal_size: (u16, u16),
        cell_size: crate::terminal_cell_size::HostCellSize,
        last_activity: u64,
        render_encoding: RenderEncoding,
        writer: Option<ClientWriter>,
    ) -> Self {
        // Only a shell connection has a surface to activate. A direct terminal
        // stream starting with this flag set would pass any check that reads the
        // flag alone (foreground promotion, tab geometry claims) as an active
        // shell; those checks go through `is_active_shell_client`, and this
        // keeps the flag itself truthful as well.
        let shell_surface_active = matches!(mode, ClientConnectionMode::ClientShell);
        Self {
            mode,
            terminal_size,
            cell_size,
            last_activity,
            render_state: ClientRenderState::new(render_encoding),
            pixel_mouse: false,
            host_terminal_theme: crate::terminal_theme::TerminalTheme::default(),
            host_terminal_appearance: None,
            host_terminal_appearance_explicit: false,
            outer_terminal_focus: None,
            host_keyboard_report_all_active: None,
            render_pending: false,
            oversized_frame_reported: false,
            attach_input_drop_reported: false,
            shell_surface_active,
            shell_mouse_capture: false,
            host_mouse_capture_active: None,
            host_sgr_pixels_active: None,
            host_keyboard_protocol_active: None,
            shell_held_inputs: HashMap::new(),
            shell_location: None,
            shell_snapshot: None,
            shell_projection_revision: 0,
            shell_endpoint_command_in_flight: false,
            writer,
        }
    }

    pub(crate) fn request_repaint(&mut self) {
        self.render_state.request_repaint();
    }

    pub(crate) fn request_recompute(&mut self) {
        self.render_state.request_recompute();
    }

    pub(crate) fn track_shell_input(&mut self, target: &str, events: &[ClientPaneInputEvent]) {
        for event in events {
            match event {
                // A press that committed text gets no release from the client,
                // so only presses without generated text are held.
                ClientPaneInputEvent::Key {
                    code,
                    modifiers,
                    kind: ClientKeyKind::Press,
                    shifted_codepoint,
                    generated_text: None,
                    ..
                } => {
                    self.shell_held_inputs.insert(
                        ClientShellPressId::Key(code.clone()),
                        ClientShellHeldInput {
                            target: target.to_owned(),
                            release: ClientPaneInputEvent::Key {
                                code: code.clone(),
                                modifiers: *modifiers,
                                kind: ClientKeyKind::Release,
                                repeat_count: 1,
                                shifted_codepoint: *shifted_codepoint,
                                generated_text: None,
                            },
                        },
                    );
                }
                ClientPaneInputEvent::Key {
                    code,
                    kind: ClientKeyKind::Release,
                    ..
                } => {
                    self.shell_held_inputs
                        .remove(&ClientShellPressId::Key(code.clone()));
                }
                ClientPaneInputEvent::Mouse {
                    kind: ClientMouseKind::Down(button),
                    position,
                    geometry,
                    modifiers,
                    ..
                }
                | ClientPaneInputEvent::Mouse {
                    kind: ClientMouseKind::Drag(button),
                    position,
                    geometry,
                    modifiers,
                    ..
                } => {
                    let id = ClientShellPressId::Mouse(*button);
                    if matches!(
                        event,
                        ClientPaneInputEvent::Mouse {
                            kind: ClientMouseKind::Down(_),
                            ..
                        }
                    ) || self.shell_held_inputs.contains_key(&id)
                    {
                        self.shell_held_inputs.insert(
                            id,
                            ClientShellHeldInput {
                                target: target.to_owned(),
                                release: ClientPaneInputEvent::Mouse {
                                    kind: ClientMouseKind::Up(*button),
                                    position: *position,
                                    geometry: *geometry,
                                    modifiers: *modifiers,
                                    lines: 1,
                                },
                            },
                        );
                    }
                }
                ClientPaneInputEvent::Mouse {
                    kind: ClientMouseKind::Up(button),
                    ..
                } => {
                    self.shell_held_inputs
                        .remove(&ClientShellPressId::Mouse(*button));
                }
                ClientPaneInputEvent::Key {
                    kind: ClientKeyKind::Press | ClientKeyKind::Repeat,
                    ..
                }
                | ClientPaneInputEvent::TextCommit(_)
                | ClientPaneInputEvent::Mouse { .. }
                | ClientPaneInputEvent::Paste(_) => {}
            }
        }
    }

    pub(crate) fn drain_shell_held_inputs(&mut self) -> Vec<ClientShellHeldInput> {
        self.shell_held_inputs
            .drain()
            .map(|(_, held)| held)
            .collect()
    }

    pub(crate) fn update_host_theme(
        &mut self,
        update: &crate::protocol::ClientHostThemeUpdate,
    ) -> bool {
        let mut next_theme = self.host_terminal_theme;
        let mut changed = false;

        match update {
            crate::protocol::ClientHostThemeUpdate::DefaultColor { kind, color } => {
                let kind = match kind {
                    crate::protocol::ClientHostDefaultColorKind::Foreground => {
                        crate::terminal_theme::DefaultColorKind::Foreground
                    }
                    crate::protocol::ClientHostDefaultColorKind::Background => {
                        crate::terminal_theme::DefaultColorKind::Background
                    }
                };
                let color = (*color).into();
                next_theme = next_theme.with_color(kind, color);
                if matches!(kind, crate::terminal_theme::DefaultColorKind::Background)
                    && !self.host_terminal_appearance_explicit
                {
                    changed |= self.set_host_appearance(Some(color.inferred_appearance()), false);
                }
            }
            crate::protocol::ClientHostThemeUpdate::PaletteColors(colors) => {
                for &(index, color) in colors {
                    next_theme = next_theme.with_palette_color(index, color.into());
                }
            }
            crate::protocol::ClientHostThemeUpdate::Appearance(appearance) => {
                let appearance = match appearance {
                    crate::protocol::ClientHostAppearance::Dark => {
                        crate::terminal_theme::HostAppearance::Dark
                    }
                    crate::protocol::ClientHostAppearance::Light => {
                        crate::terminal_theme::HostAppearance::Light
                    }
                };
                changed |= self.set_host_appearance(Some(appearance), true);
            }
        }

        if next_theme != self.host_terminal_theme {
            self.host_terminal_theme = next_theme;
            changed = true;
        }
        changed
    }

    fn set_host_appearance(
        &mut self,
        appearance: Option<crate::terminal_theme::HostAppearance>,
        explicit: bool,
    ) -> bool {
        if self.host_terminal_appearance_explicit && !explicit {
            return false;
        }
        if self.host_terminal_appearance == appearance
            && self.host_terminal_appearance_explicit == explicit
        {
            return false;
        }
        self.host_terminal_appearance = appearance;
        self.host_terminal_appearance_explicit = explicit;
        true
    }

    pub(crate) fn deferred_render(&self) -> DeferredRender {
        if self.render_pending {
            DeferredRender::Full
        } else {
            DeferredRender::None
        }
    }

    pub(crate) fn clear_deferred_render(&mut self) {
        self.render_pending = false;
    }

    pub(crate) fn defer_full_render(&mut self) {
        self.render_pending = true;
    }

    pub(crate) fn take_deferred_render(&mut self) -> DeferredRender {
        let deferred = self.deferred_render();
        self.clear_deferred_render();
        deferred
    }

    pub(crate) fn is_shell_client(&self) -> bool {
        matches!(self.mode, ClientConnectionMode::ClientShell)
    }

    pub(crate) fn is_active_shell_client(&self) -> bool {
        self.is_shell_client() && self.shell_surface_active
    }
}

pub(crate) fn latest_shell_client(clients: &HashMap<u64, ClientConnection>) -> Option<u64> {
    clients
        .iter()
        .filter(|(_, client)| client.is_active_shell_client())
        .max_by_key(|(_, client)| client.last_activity)
        .map(|(&client_id, _)| client_id)
}

pub(crate) fn terminal_stream_client_ids(
    clients: &HashMap<u64, ClientConnection>,
    terminal_id: &str,
) -> Vec<u64> {
    clients
        .iter()
        .filter_map(|(&client_id, client)| match &client.mode {
            ClientConnectionMode::TerminalAttach {
                terminal_id: attached,
            } if attached == terminal_id => Some(client_id),
            _ => None,
        })
        .collect()
}

pub(crate) fn render_targets(
    clients: &HashMap<u64, ClientConnection>,
    foreground_client_id: Option<u64>,
) -> Vec<RenderTarget> {
    let mut targets: Vec<RenderTarget> = clients
        .iter()
        .filter(|(_, client)| {
            client.writer.is_some()
                && (client.is_shell_client()
                    || matches!(client.mode, ClientConnectionMode::TerminalAttach { .. }))
        })
        .map(|(&client_id, client)| {
            (
                client_id,
                client.terminal_size,
                client.cell_size,
                foreground_client_id == Some(client_id),
                client.mode.clone(),
            )
        })
        .collect();

    targets.sort_by_key(|(client_id, _, _, is_foreground, _)| (*is_foreground, *client_id));
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell_client() -> ClientConnection {
        ClientConnection::new(
            (80, 24),
            crate::terminal_cell_size::HostCellSize::default(),
            1,
            crate::protocol::RenderEncoding::SemanticFrame,
            None,
        )
    }

    #[test]
    fn only_shell_connections_start_with_an_active_surface() {
        let connection = |mode| {
            ClientConnection::new_with_mode(
                mode,
                (80, 24),
                crate::terminal_cell_size::HostCellSize::default(),
                1,
                crate::protocol::RenderEncoding::TerminalAnsi,
                None,
            )
        };
        assert!(connection(ClientConnectionMode::ClientShell).shell_surface_active);
        let pending = connection(ClientConnectionMode::TerminalPending);
        assert!(!pending.shell_surface_active);
        assert!(!pending.is_active_shell_client());
        let attached = connection(ClientConnectionMode::TerminalAttach {
            terminal_id: "t1".into(),
        });
        assert!(!attached.shell_surface_active);
        assert!(!attached.is_active_shell_client());

        let mut clients = HashMap::new();
        clients.insert(1, pending);
        clients.insert(2, attached);
        assert_eq!(latest_shell_client(&clients), None);
    }

    #[test]
    fn semantic_text_press_does_not_create_a_server_release_lease() {
        let mut client = shell_client();
        client.track_shell_input(
            "w1:p1",
            &[ClientPaneInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Char('x'),
                modifiers: 0,
                kind: ClientKeyKind::Press,
                repeat_count: 1,
                shifted_codepoint: None,
                generated_text: Some("x".into()),
            }],
        );

        assert!(client.drain_shell_held_inputs().is_empty());
    }

    #[test]
    fn held_key_presses_are_released_once_per_key_code() {
        let mut client = shell_client();
        let key = |code, kind| ClientPaneInputEvent::Key {
            code,
            modifiers: crossterm::event::KeyModifiers::SHIFT.bits(),
            kind,
            repeat_count: 1,
            shifted_codepoint: None,
            generated_text: None,
        };
        client.track_shell_input(
            "w1:p1",
            &[
                key(crate::protocol::ClientKeyCode::Enter, ClientKeyKind::Press),
                key(crate::protocol::ClientKeyCode::Enter, ClientKeyKind::Press),
                key(crate::protocol::ClientKeyCode::Esc, ClientKeyKind::Press),
                key(crate::protocol::ClientKeyCode::Esc, ClientKeyKind::Release),
            ],
        );

        let held = client.drain_shell_held_inputs();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].target, "w1:p1");
        assert_eq!(
            held[0].release,
            key(
                crate::protocol::ClientKeyCode::Enter,
                ClientKeyKind::Release
            )
        );
    }
}
