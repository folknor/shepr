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

    /// Admits the event once; every path below takes the admitted value, so
    /// no envelope is rebuilt between admissions. A death's generation is
    /// looked up once more by `App::handle_prepared_pane_exit` before the
    /// removal; nothing between the two can change the pane's runtime, and
    /// neither check has an effect, so the event is still applied once.
    fn handle_internal_event_with_origin(&mut self, ev: AppEvent, origin: EventOrigin) -> bool {
        let Some(admitted) = self.app.admit_event(ev) else {
            return false;
        };
        let pane = match admitted {
            app::Admitted::Pane(pane) => pane,
            git @ app::Admitted::GitRefreshed(_) => {
                return self.app.handle_admitted_event(git);
            }
        };
        match pane.into_death() {
            Ok(death) => self.handle_admitted_pane_death(death, origin),
            Err(pane) => {
                if let shepr_mux::events::RuntimeEvent::ClipboardWrite { content } = &pane.event {
                    // Clipboard writes are client-local side effects. They go to
                    // the clients viewing the writing pane, or, when none views
                    // it (a hidden workspace, a background program), to the foreground
                    // client so the write is not lost.
                    let message = ServerMessage::Clipboard {
                        data: content.clone(),
                    };
                    let viewers = self.pane_viewers(pane.pane_id);
                    if viewers.is_empty() {
                        self.send_to_foreground_client(&message);
                    } else {
                        for client_id in viewers {
                            self.send_to_client(client_id, &message);
                        }
                    }
                    return false;
                }
                self.app.handle_admitted_event(app::Admitted::Pane(pane))
            }
        }
    }

    fn handle_admitted_pane_death(&mut self, death: app::PaneDeath, origin: EventOrigin) -> bool {
        // After a termination signal, the panes are most likely dying from the
        // same teardown. Removing them would save a session with panes missing.
        // The checkpoint taken before a signal-killed pane is removed does not
        // help with shells that catch the signal and exit with a status code.
        // Leave the layout as it is for the final save; the process is about
        // to exit anyway.
        if self.lifecycle.signal_quit_requested() {
            return false;
        }
        // Publishing the process exit can change what the sidebar shows
        // (the agent goes idle) even when the pane itself stays, held for
        // its checkpoint or not removed at all.
        let mut projection_changed = false;
        let prepared = match origin {
            // A replayed exit was held for its checkpoint, so it was
            // decided as checkpointed; it is finished that way whatever
            // has happened to the pane since.
            EventOrigin::Replay(generation) => {
                if !self.app.pane_exit_checkpoint_generation_settled(generation) {
                    self.pending_checkpointed_pane_exits
                        .push_back(PendingCheckpointedPaneExit {
                            event: death.into_envelope(),
                            checkpoint_generation: generation,
                        });
                    return false;
                }
                app::PreparedPaneExit::replayed(death, generation)
            }
            EventOrigin::Fresh => {
                let app::PaneExitPrepared {
                    prepared,
                    projection_changed: changed,
                } = self.app.prepare_pane_exit(death);
                projection_changed = changed;
                if let Some(checkpoint_generation) = prepared.held_generation() {
                    // Keep the pre-exit layout live until its checkpoint is durable.
                    self.pending_checkpointed_pane_exits
                        .push_back(PendingCheckpointedPaneExit {
                            event: prepared.into_envelope(),
                            checkpoint_generation,
                        });
                    return projection_changed;
                }
                prepared
            }
        };

        if !self.app.handle_prepared_pane_exit(&prepared) {
            return projection_changed;
        }
        self.immediate_pty_sources_dirty = true;
        self.host_input_modes_dirty = true;
        self.reconcile_client_shell_locations();
        self.sync_pane_focus();
        self.reapply_controlled_shell_workspace_geometry(client_views::PendingResumes::Defer);

        true
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
        let queued_on_entry = self.outputs.queued_events();
        self.drain_internal_events_with_forwarding_up_to(queued_on_entry)
    }

    pub(super) fn drain_internal_events_with_forwarding_up_to(&mut self, limit: usize) -> bool {
        // Check once per batch before applying any event in it. The monitor
        // wakes the loop when this flag changes, so a later batch observes it.
        self.lifecycle.sync_host_shutdown_freeze(&mut self.app);
        let mut changed = false;
        for _ in 0..limit {
            let Some(ev) = self.outputs.try_next_event() else {
                break;
            };
            changed |= self.handle_internal_event_with_forwarding(ev);
        }
        changed
    }
}
