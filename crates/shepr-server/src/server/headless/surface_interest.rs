use super::*;
use crate::server::ClientId;

impl HeadlessServer {
    /// Apply a client-shell surface lease and return the resulting projection floor.
    ///
    /// Every `active: true` request advances the floor, even if the server already considered
    /// the connection active. That makes a new client activation epoch distinguishable from a
    /// delayed same-boot PaneSurface that was prepared for an earlier epoch.
    pub(super) fn set_client_shell_surface_active(
        &mut self,
        client_id: ClientId,
        active: bool,
    ) -> Option<(bool, u64)> {
        // The floor is per connection and steps once per activation or
        // changed snapshot, so exhaustion is unreachable in practice. Should
        // it happen, drop the client: it reconnects with a fresh counter
        // instead of receiving a floor that repeats a revision.
        let raised_floor = if active {
            let current = self
                .clients
                .get(&client_id)?
                .shell_state()
                .projection_revision;
            let Some(raised) = current.checked_next() else {
                warn!(
                    ?client_id,
                    "projection revisions exhausted; dropping client"
                );
                self.remove_client_and_resize_if_needed(client_id);
                return None;
            };
            Some(raised)
        } else {
            None
        };
        let (changed, projection_revision, held_inputs) = {
            let client = self.clients.get_mut(&client_id)?;
            let (changed, projection_revision) = {
                let shell = client.shell_state_mut();
                let changed = shell.surface_active != active;
                if let Some(raised) = raised_floor {
                    shell.projection_revision = raised;
                    // Force the next control snapshot to carry this new floor instead of reusing a
                    // same-boot cached snapshot from the prior surface epoch.
                    shell.snapshot = None;
                }
                if !changed && !active {
                    return Some((false, shell.projection_revision.get()));
                }
                shell.surface_active = active;
                (changed, shell.projection_revision)
            };
            client.request_repaint();
            // The client drops target effects while its old source frame is frozen. Reset the
            // dedupe state whenever a viewer is (re)activated so the post-commit replay can
            // produce mouse/keyboard modes and graphics even when runtime demand is unchanged.
            if active {
                client.host_mouse_capture_active = None;
                client.host_sgr_pixels_active = None;
                client.shell_state_mut().host_keyboard_report_all_active = None;
                // Do not emit/cache a title during a frozen target activation.
                // A committed client explicitly requests the bounded replay
                // after its coherent frame is visible.
                client.sent_window_title = None;
            }
            client.clear_deferred_render();
            if !active && let Some(writer) = &client.writer {
                writer.discard_pending_render();
            }
            let held_inputs = (!active && changed).then(|| client.drain_shell_held_inputs());
            (changed, projection_revision, held_inputs)
        };

        if let Some(held_inputs) = held_inputs {
            self.release_client_shell_inputs(client_id, held_inputs);
        }

        if active {
            self.promote_client_to_foreground(client_id);
            // A surface that already sizes some workspace settles it now,
            // starting any resume the geometry was holding back.
            self.resize_shell_workspaces_sized_for(client_id, true);
            let focused_viewer_already_owns_workspace = self
                .shell_target_for_client(client_id)
                .is_some_and(|workspace_id| {
                    self.clients.iter().any(|(&other_id, client)| {
                        other_id != client_id
                            && client.is_active_shell_client()
                            && client.shell_state().outer_terminal_focus == Some(true)
                            && self.shell_target_for_client(other_id).as_ref()
                                == Some(&workspace_id)
                    })
                });
            if !focused_viewer_already_owns_workspace {
                self.claim_shell_workspace_geometry(client_id, true);
            }
        } else {
            self.clients.remove_geometry_controllers_for(client_id);
            if self.clients.foreground_client_id() == Some(client_id) {
                self.promote_latest_remaining_client();
            }
            self.reapply_controlled_shell_workspace_geometry(true);
        }
        // An inactive surface holds no pane focus; an active one does again.
        self.sync_pane_focus();
        Some((changed || active, projection_revision.get()))
    }
}
