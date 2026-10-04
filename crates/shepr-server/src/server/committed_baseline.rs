//! What one client was last sent, in the form later render paths work from.

use crate::app::AppState;
use crate::server::clients::ClientPaneIdentity;
use crate::ui::{PaneLayoutCache, PaneSurface};
use shepr_protocol::{PaneSurfaceFrame, PaneSurfacePane};

/// A client's committed surface and the typed identity of each of its panes.
/// The two are committed together and dropped together, so the identities
/// always describe the panes of the surface they sit beside. A public pane
/// id can outlive a layout update with no change to the wire fields, but its
/// internal pane identity still belongs in what retained rendering trusts.
pub(crate) struct CommittedBaseline {
    surface: PaneSurfaceFrame,
    /// Aligned with `surface.panes`.
    identities: Vec<ClientPaneIdentity>,
}

/// One pane of a committed surface checked against the current layout.
pub(crate) struct CommittedPane<'a> {
    /// The pane as committed to the client.
    pub(crate) wire: &'a PaneSurfacePane,
    pub(crate) identity: &'a ClientPaneIdentity,
    /// How the pane looks now, settled for its committed screen mode. Its
    /// scrollbar is not decided yet: that waits for fresh scroll metrics.
    pub(crate) look: PaneSurface,
}

impl CommittedBaseline {
    pub(crate) fn new(surface: PaneSurfaceFrame, identities: Vec<ClientPaneIdentity>) -> Self {
        Self {
            surface,
            identities,
        }
    }

    pub(crate) fn surface(&self) -> &PaneSurfaceFrame {
        &self.surface
    }

    pub(crate) fn surface_mut(&mut self) -> &mut PaneSurfaceFrame {
        &mut self.surface
    }

    /// Whether `identities` are the ones committed with the surface.
    pub(crate) fn has_identities(&self, identities: &[ClientPaneIdentity]) -> bool {
        self.identities == identities
    }

    /// The committed panes, each paired with its identity and how the ui lays
    /// it out now. `None` when the committed surface no longer matches the
    /// current layout, which sends the client to a full render: the identities
    /// are not one per pane or span workspaces, the workspace is gone, or a
    /// pane's identity or committed rects differ from the layout's.
    pub(crate) fn panes<'a>(
        &'a self,
        state: &AppState,
        layouts: &mut PaneLayoutCache,
    ) -> Option<Vec<CommittedPane<'a>>> {
        let panes = &self.surface.panes;
        if panes.len() != self.identities.len() {
            return None;
        }
        let Some(first) = self.identities.first() else {
            return Some(Vec::new());
        };
        if self
            .identities
            .iter()
            .any(|identity| identity.workspace_id != first.workspace_id)
        {
            return None;
        }
        let chromes = layouts.chromes(
            state,
            first.workspace_id,
            self.surface.frame.width(),
            self.surface.frame.height(),
        )?;
        if chromes.len() != panes.len() {
            return None;
        }
        let mut resolved = Vec::with_capacity(panes.len());
        for ((wire, identity), chrome) in panes.iter().zip(&self.identities).zip(chromes) {
            if chrome.id != identity.pane_id {
                return None;
            }
            let look = PaneSurface::settle(
                chrome.clone(),
                state.settings().pane_scrollbars,
                wire.alternate_screen_active,
                || None,
            );
            if !look.matches_committed(wire) {
                return None;
            }
            resolved.push(CommittedPane {
                wire,
                identity,
                look,
            });
        }
        Some(resolved)
    }
}
