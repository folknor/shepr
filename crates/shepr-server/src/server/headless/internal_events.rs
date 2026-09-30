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
        let runtime_origin = match &ev {
            AppEvent::Runtime {
                pane_id,
                generation,
                ..
            } => Some((*pane_id, *generation)),
            _ => None,
        };
        let Some(ev) = self.app.admit_runtime_event(ev) else {
            self.replaying_checkpointed_pane_exit = None;
            return false;
        };
        // A host shutdown warning that arrived since the loop last looked is
        // answered with its checkpoint before this event can change the
        // layout; after that, saving is frozen and events apply normally.
        self.sync_host_shutdown_freeze(self.app.clock.now);
        // After a termination signal, the panes are most likely dying from the
        // same teardown. Removing them would save a session with panes missing.
        // The checkpoint taken before a signal-killed pane is removed does not
        // help with shells that catch the signal and exit with a status code.
        // Leave the layout as it is for the final save; the process is about
        // to exit anyway.
        if matches!(ev, AppEvent::PaneDied { .. }) && self.lifecycle.signal_quit_requested() {
            self.replaying_checkpointed_pane_exit = None;
            return false;
        }
        match &ev {
            AppEvent::ClipboardWrite { pane_id, content } => {
                // Clipboard writes are client-local side effects. They go to
                // the clients viewing the writing pane, or, when none views
                // it (a hidden workspace, a background program), to the foreground
                // client so the write is not lost.
                let data = base64::engine::general_purpose::STANDARD.encode(content.as_slice());
                let message = ServerMessage::Clipboard { data };
                let viewers = self.clipboard_viewers(*pane_id);
                if viewers.is_empty() {
                    self.send_to_foreground_client(&message);
                } else {
                    for client_id in viewers {
                        self.send_to_client(client_id, &message);
                    }
                }
                false
            }
            AppEvent::PaneDied {
                pane_id,
                exit_reason,
            } => {
                // Publishing the process exit can change what the sidebar shows
                // (the agent goes idle) even when the pane itself stays, held for
                // its checkpoint or not removed at all.
                let projection_before = self.app.state.shell_projection_revision;
                let replay_generation = self.replaying_checkpointed_pane_exit.take();
                if let Some(generation) = replay_generation {
                    if !self.app.pane_exit_checkpoint_generation_settled(generation) {
                        self.pending_checkpointed_pane_exits.push_back(
                            PendingCheckpointedPaneExit {
                                event: match runtime_origin {
                                    Some((pane_id, generation)) => AppEvent::Runtime {
                                        pane_id,
                                        generation,
                                        event: Box::new(ev),
                                    },
                                    None => ev,
                                },
                                checkpoint_generation: generation,
                            },
                        );
                        return false;
                    }
                } else if let Some(checkpoint_generation) =
                    self.app.prepare_pane_exit(*pane_id, *exit_reason)
                {
                    // Keep the pre-exit layout live until its checkpoint is durable.
                    self.pending_checkpointed_pane_exits
                        .push_back(PendingCheckpointedPaneExit {
                            event: match runtime_origin {
                                Some((pane_id, generation)) => AppEvent::Runtime {
                                    pane_id,
                                    generation,
                                    event: Box::new(ev),
                                },
                                None => ev,
                            },
                            checkpoint_generation,
                        });
                    return self.app.state.shell_projection_revision != projection_before;
                }

                if self.app.handle_prepared_pane_exit(ev) == RenderDemand::None {
                    return self.app.state.shell_projection_revision != projection_before;
                }
                self.immediate_pty_sources_dirty = true;
                self.host_input_modes_dirty = true;
                self.reconcile_client_shell_locations();
                self.sync_pane_focus();
                self.reapply_controlled_shell_workspace_geometry(false);

                true
            }
            _ => self.app.handle_internal_event_with_render_demand(ev) != RenderDemand::None,
        }
    }

    /// Drains internal events, forwarding clipboard writes to connected
    /// clients instead of processing them locally.
    ///
    /// The server has no host terminal, so we forward `ClipboardWrite` as
    /// `ServerMessage::Clipboard` to the clients viewing the writing pane.
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
