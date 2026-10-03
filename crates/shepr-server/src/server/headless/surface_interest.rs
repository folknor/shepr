use super::*;
use crate::server::ClientId;
use crate::server::clients::ClientSurfaceChange;

impl HeadlessServer {
    /// Set whether this connection is viewed and return the resulting projection floor.
    ///
    /// Every `active: true` request advances the floor, even if the server already considered
    /// the connection viewed. That makes a new viewing epoch distinguishable from a delayed
    /// same-boot PaneSurface that was prepared for an earlier epoch.
    pub(super) fn set_client_shell_surface_active(
        &mut self,
        client_id: ClientId,
        active: bool,
    ) -> Option<(bool, shepr_protocol::ProjectionRevision)> {
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
                if let Some(client) = self.clients.get(&client_id) {
                    client.outbox.close();
                }
                return None;
            };
            Some(raised)
        } else {
            None
        };
        let change = self.clients.set_surface_active(client_id, active)?;
        let ClientSurfaceChange {
            changed,
            foreground_changed,
            departure,
        } = change;
        let projection_revision = {
            let client = self.clients.get_mut(&client_id)?;
            let projection_revision = {
                let shell = client.shell_state_mut();
                if let Some(raised) = raised_floor {
                    shell.projection_revision = raised;
                    // Force the next control snapshot to carry this new floor instead of reusing a
                    // same-boot cached snapshot from the prior surface epoch.
                    shell.snapshot = None;
                }
                if !changed && !active {
                    return Some((false, shell.projection_revision));
                }
                shell.projection_revision
            };
            client.request_repaint();
            // The client drops a target's effects until it commits. Reset the dedupe state
            // whenever a viewer is (re)activated so the post-commit replay can produce
            // mouse/keyboard modes and graphics even when runtime demand is unchanged.
            if active {
                // Do not emit/cache a title while a target is being prepared.
                // A committed client explicitly requests the bounded replay
                // with ReplayHostEffects after its coherent frame is visible.
                client.outbox.forget_presentation();
            }
            if !active {
                client.outbox.discard_pending_surface();
            }
            projection_revision
        };

        if active {
            self.refresh_client_view_keys();
            if foreground_changed {
                self.sync_host_theme_from_foreground();
            }
            // A surface that already sizes some workspace settles it now,
            // starting any resume the geometry was holding back.
            self.resize_shell_workspaces_sized_for(client_id, true);
            let focused_viewer_already_owns_workspace = self
                .shell_target_for_client(client_id)
                .is_some_and(|workspace_id| {
                    self.clients.presenting().any(|(&other_id, client)| {
                        other_id != client_id
                            && client.shell_state().outer_terminal_focus == Some(true)
                            && self.shell_target_for_client(other_id).as_ref()
                                == Some(&workspace_id)
                    })
                });
            if !focused_viewer_already_owns_workspace {
                self.claim_shell_workspace_geometry(client_id, true);
            }
        } else {
            if let Some(departure) = departure {
                self.apply_client_departures(vec![(client_id, departure)]);
            }
        }
        // An inactive surface holds no pane focus; an active one does again.
        if active {
            self.sync_pane_focus();
        }
        Some((changed || active, projection_revision))
    }
}
