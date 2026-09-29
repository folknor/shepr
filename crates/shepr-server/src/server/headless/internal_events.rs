use super::*;

impl HeadlessServer {
    /// Handles a single internal event with forwarding logic for clipboard
    /// writes to connected clients.
    ///
    /// Every internal event the headless server handles is routed through
    /// this method, so clipboard forwarding is never bypassed; the
    /// `headless-internal-events-go-through-forwarding` textlint refuses a
    /// direct call to the app's internal-event handlers anywhere else under
    /// `crates/shepr-server/src/server/`.
    ///
    /// Returns true if the event changed visual state (requiring a re-render).
    pub(super) fn handle_internal_event_with_forwarding(&mut self, ev: AppEvent) -> bool {
        // A host shutdown warning that arrived since the loop last looked is
        // answered with its checkpoint before this event can change the
        // layout; after that, saving is frozen and events apply normally.
        self.sync_host_shutdown_freeze(self.app.clock.now);
        self.immediate_pty_sources_dirty = true;
        // After a termination signal, the panes are most likely dying from the
        // same teardown. Removing them would save a session with panes missing.
        // The checkpoint taken before a signal-killed pane is removed does not
        // help with shells that catch the signal and exit with a status code.
        // Leave the layout as it is for the final save; the process is about
        // to exit anyway.
        if matches!(ev, AppEvent::PaneDied { .. }) && self.lifecycle.signal_quit_requested() {
            return false;
        }
        match &ev {
            AppEvent::ClipboardWrite { content } => {
                // Clipboard writes are client-local side effects. Forward them only to
                // the foreground client instead of broadcasting to every attached client.
                let data = base64::engine::general_purpose::STANDARD.encode(content.as_slice());
                self.send_to_foreground_client(&ServerMessage::Clipboard { data });
                false
            }
            // Agent state and hook reports need the latest outer-terminal focus
            // before application code runs; neither changes geometry, so the
            // view is not recomputed for them.
            AppEvent::StateChanged { .. } | AppEvent::HookStateReported { .. } => {
                self.sync_foreground_focus_state();
                self.app.handle_internal_event_with_pane_updates(ev);
                true
            }
            AppEvent::PaneDied {
                pane_id,
                exit_reason,
            } => {
                let focus_before = self.shell_focus_targets();
                let focused_tabs_before = self.focused_shell_tabs();
                let pane_id_val = *pane_id;
                if let Some(update) = self
                    .app
                    .state
                    .publish_pane_process_exit_if_agent(pane_id_val)
                {
                    self.app.sync_full_lifecycle_authority_detection_pauses();
                    self.app.emit_pane_state_update(&update);
                    // The agent row changes even when removal waits for its
                    // checkpoint below.
                    self.app.state.mark_shell_projection_dirty();
                }

                if exit_reason.requires_session_checkpoint()
                    && self
                        .app
                        .state
                        .prepare_pane_removal_by_id(pane_id_val)
                        .is_some()
                    && !self.app.checkpoint_session_before_pane_exit()
                {
                    // Keep the pre-exit layout live until its checkpoint is durable.
                    self.pending_checkpointed_pane_exits.push_back(ev);
                    return false;
                }

                self.app.handle_internal_event_with_pane_updates(ev);
                self.reconcile_client_shell_locations();
                self.finish_shell_location_reconciliation(focus_before, &focused_tabs_before);
                self.reapply_controlled_shell_tab_geometry(false);

                true
            }
            _ => self.app.handle_internal_event_with_render_demand(ev) != RenderDemand::None,
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
        // Drain the events already queued when this API request reached the
        // loop. Producers may keep adding events while we process them, but
        // those belong to the next loop turn so they cannot starve the request.
        let queued_on_entry = self.app.event_rx.len();
        self.drain_internal_events_with_forwarding_up_to(queued_on_entry)
            .1
    }

    pub(super) fn drain_internal_events_with_forwarding_up_to(
        &mut self,
        limit: usize,
    ) -> (bool, bool) {
        let mut had_event = false;
        let mut changed = false;
        for _ in 0..limit {
            let Ok(ev) = self.app.event_rx.try_recv() else {
                break;
            };
            had_event = true;
            changed |= self.handle_internal_event_with_forwarding(ev);
        }
        (had_event, changed)
    }
}
