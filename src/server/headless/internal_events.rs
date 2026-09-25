use super::*;

impl HeadlessServer {
    /// Handles a single internal event with forwarding logic for clipboard
    /// writes to connected clients.
    ///
    /// ALL internal events MUST be routed through this method to ensure
    /// clipboard forwarding is never bypassed. Do not call
    /// `self.app.handle_internal_event()` directly for any internal event
    /// in the headless server — use this method instead.
    ///
    /// Returns true if the event changed visual state (requiring a re-render).
    pub(super) fn handle_internal_event_with_forwarding(&mut self, ev: AppEvent) -> bool {
        if self.host_shutdown_requested.load(Ordering::Acquire) {
            return false;
        }
        match &ev {
            AppEvent::ClipboardWrite { content } => {
                // Clipboard writes are client-local side effects. Forward them only to
                // the foreground client instead of broadcasting to every attached client.
                let data = base64::engine::general_purpose::STANDARD.encode(content.as_slice());
                self.send_to_foreground_client(ServerMessage::Clipboard { data });
                false
            }
            AppEvent::StateChanged { .. } => {
                self.sync_foreground_client_state();
                self.app.handle_internal_event_with_pane_updates(ev);
                true
            }
            AppEvent::HookStateReported { .. } => {
                self.sync_foreground_client_state();
                self.app.handle_internal_event_with_pane_updates(ev);
                true
            }
            AppEvent::PaneDied { pane_id, .. } => {
                let focus_before = self.shell_focus_targets();
                let focused_tabs_before = self.focused_shell_tabs();
                let pane_id_val = *pane_id;
                let terminal_id = self.app.state.workspaces.iter().find_map(|ws| {
                    ws.tabs.iter().find_map(|tab| {
                        tab.panes
                            .get(pane_id)
                            .map(|pane| pane.attached_terminal_id.to_string())
                    })
                });
                if let Some(update) = self
                    .app
                    .state
                    .publish_pane_process_exit_if_agent(pane_id_val, false)
                {
                    self.app.emit_pane_state_update(&update);
                }

                self.app.handle_internal_event_with_pane_updates(ev);
                self.reconcile_client_shell_locations();
                self.finish_shell_location_reconciliation(focus_before, &focused_tabs_before);
                self.reapply_controlled_shell_tab_geometry(false);

                if self.app.find_pane(pane_id_val).is_none() {
                    if let Some(terminal_id) = terminal_id {
                        self.shutdown_terminal_stream_clients(
                            &terminal_id,
                            format!("terminal {terminal_id} exited"),
                        );
                    }
                }

                true
            }
            _ => self.app.handle_internal_event_with_render_impact(ev),
        }
    }

    /// Drains internal events, forwarding clipboard writes to connected
    /// clients instead of processing them locally.
    ///
    /// The server has no host terminal, so we forward `ClipboardWrite` as
    /// `ServerMessage::Clipboard` to the foreground client only.
    pub(super) fn drain_internal_events_with_forwarding(&mut self) -> bool {
        self.drain_internal_events_with_forwarding_up_to(crate::app::APP_EVENT_DRAIN_LIMIT)
            .1
    }

    pub(super) fn drain_all_internal_events_with_forwarding(&mut self) -> bool {
        let mut changed = false;
        loop {
            let (had_event, batch_changed) =
                self.drain_internal_events_with_forwarding_up_to(crate::app::APP_EVENT_DRAIN_LIMIT);
            changed |= batch_changed;
            if !had_event || self.should_quit.load(Ordering::Acquire) {
                break;
            }
        }
        changed
    }

    pub(super) fn drain_internal_events_with_forwarding_up_to(
        &mut self,
        limit: usize,
    ) -> (bool, bool) {
        let mut had_event = false;
        let mut changed = false;
        for _ in 0..limit {
            if self.host_shutdown_requested.load(Ordering::Acquire) {
                break;
            }
            let Ok(ev) = self.app.event_rx.try_recv() else {
                break;
            };
            had_event = true;
            changed |= self.handle_internal_event_with_forwarding(ev);
        }
        (had_event, changed)
    }
}
