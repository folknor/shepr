use super::*;

#[derive(Clone, Copy)]
pub(super) enum EventOrigin {
    Fresh,
    Replay(app::CheckpointGeneration),
}

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
        self.handle_internal_event_with_origin(ev, EventOrigin::Fresh)
    }

    /// Replays an event held for a checkpoint, through the same forwarding
    /// path as every other internal event.
    pub(super) fn replay_checkpointed_internal_event(
        &mut self,
        ev: AppEvent,
        checkpoint_generation: app::CheckpointGeneration,
    ) -> bool {
        self.handle_internal_event_with_origin(ev, EventOrigin::Replay(checkpoint_generation))
    }

    fn handle_internal_event_with_origin(&mut self, ev: AppEvent, origin: EventOrigin) -> bool {
        let runtime_origin = match &ev {
            AppEvent::Runtime {
                pane_id,
                generation,
                ..
            } => Some((*pane_id, *generation)),
            _ => None,
        };
        let Some(ev) = self.app.admit_runtime_event(ev) else {
            return false;
        };
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
            AppEvent::ClipboardWrite { pane_id, content } => {
                // Clipboard writes are client-local side effects. They go to
                // the clients viewing the writing pane, or, when none views
                // it (a hidden workspace, a background program), to the foreground
                // client so the write is not lost.
                let message = ServerMessage::Clipboard {
                    data: content.clone(),
                };
                let viewers = self.pane_viewers(*pane_id);
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
                ended_at,
            } => {
                // Publishing the process exit can change what the sidebar shows
                // (the agent goes idle) even when the pane itself stays, held for
                // its checkpoint or not removed at all.
                let replay_generation = match origin {
                    EventOrigin::Fresh => None,
                    EventOrigin::Replay(generation) => Some(generation),
                };
                // A replayed exit was held for its checkpoint, so it was
                // decided as checkpointed; it is finished that way whatever
                // has happened to the pane since.
                let mut projection_changed = false;
                let prepared = if let Some(generation) = replay_generation {
                    if !self.app.pane_exit_checkpoint_generation_settled(generation) {
                        self.pending_checkpointed_pane_exits.push_back(
                            PendingCheckpointedPaneExit {
                                event: preserve_runtime_origin(runtime_origin, ev),
                                checkpoint_generation: generation,
                            },
                        );
                        return false;
                    }
                    crate::app::PreparedPaneExit::Held(generation)
                } else {
                    let (prepared, changed) = self.app.observe_projection_change(|app| {
                        app.prepare_pane_exit(*pane_id, *exit_reason, *ended_at)
                    });
                    projection_changed = changed;
                    if let Some(checkpoint_generation) = prepared.held_generation() {
                        // Keep the pre-exit layout live until its checkpoint is durable.
                        self.pending_checkpointed_pane_exits.push_back(
                            PendingCheckpointedPaneExit {
                                event: preserve_runtime_origin(runtime_origin, ev),
                                checkpoint_generation,
                            },
                        );
                        return projection_changed;
                    }
                    prepared
                };

                if !self.app.handle_prepared_pane_exit(
                    preserve_runtime_origin(runtime_origin, ev),
                    prepared,
                ) {
                    return projection_changed;
                }
                self.immediate_pty_sources_dirty = true;
                self.host_input_modes_dirty = true;
                self.reconcile_client_shell_locations();
                self.sync_pane_focus();
                self.reapply_controlled_shell_workspace_geometry(false);

                true
            }
            _ => self
                .app
                .handle_internal_event_with_view_change(preserve_runtime_origin(
                    runtime_origin,
                    ev,
                )),
        }
    }

    /// Drains internal events, forwarding clipboard writes to connected
    /// clients instead of processing them locally.
    ///
    /// The server has no host terminal, so we forward `ClipboardWrite` as
    /// `ServerMessage::Clipboard` to the clients viewing the writing pane.
    pub(super) fn drain_internal_events_with_forwarding(&mut self) -> bool {
        self.drain_internal_events_with_forwarding_up_to(crate::app::APP_EVENT_DRAIN_LIMIT)
    }

    pub(super) fn drain_all_internal_events_with_forwarding(&mut self) -> bool {
        // Drain the events already queued when this API request reached the
        // loop. Producers may keep adding events while we process them, but
        // those belong to the next loop turn so they cannot starve the request.
        let queued_on_entry = self.app.event_rx.len();
        self.drain_internal_events_with_forwarding_up_to(queued_on_entry)
    }

    pub(super) fn drain_internal_events_with_forwarding_up_to(&mut self, limit: usize) -> bool {
        // Check once per batch before applying any event in it. The monitor
        // wakes the loop when this flag changes, so a later batch observes it.
        self.lifecycle.sync_host_shutdown_freeze(&mut self.app);
        let mut changed = false;
        for _ in 0..limit {
            let Ok(ev) = self.app.event_rx.try_recv() else {
                break;
            };
            changed |= self.handle_internal_event_with_forwarding(ev);
        }
        changed
    }
}

/// Keep runtime identity attached while a pane exit waits so admission checks
/// the same producer again when the queued event is replayed. An event that
/// came out of a runtime envelope is always a runtime payload; anything else
/// is returned bare, which admission refuses rather than misattributes.
fn preserve_runtime_origin(
    origin: Option<(
        shepr_core::layout::PaneId,
        shepr_mux::events::RuntimeGeneration,
    )>,
    event: AppEvent,
) -> AppEvent {
    let Some((pane_id, generation)) = origin else {
        return event;
    };
    match shepr_mux::events::RuntimeEvent::try_from(event) {
        Ok(payload) => AppEvent::Runtime {
            pane_id,
            generation,
            event: Box::new(payload),
        },
        Err(event) => event,
    }
}
