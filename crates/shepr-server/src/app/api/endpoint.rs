//! What an endpoint handler produces and how it refuses.
//!
//! The endpoint path answers with `EndpointError`, the wire error of the
//! client protocol, and never touches the JSON API's `ApiError`: every app
//! refusal is `Rejected` with a message for the user, and the other variants
//! belong to the server loop.

use crate::app::App;
use shepr_core::layout::PaneId;
use shepr_protocol::command::{EndpointError, EndpointReply};
use shepr_protocol::{PublicPaneId, WorkspaceId};

/// The shared effects of one endpoint command.
///
/// These describe committed changes, not what a command could change according
/// to its protocol traits. The headless loop uses the topology fields to decide
/// whether client PTY sources need reconciling, while the app uses the
/// projection and surface fields to request rendering.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EndpointEffects {
    /// The client-shell snapshot derived from shared app state changed.
    pub(crate) shell_projection_changed: bool,
    /// A rendered pane surface changed without necessarily changing the shell
    /// snapshot (for example, scroll position or split geometry).
    pub(crate) pane_surface_changed: bool,
    /// A workspace's focused pane changed.
    pub(crate) focus_changed: bool,
    /// A workspace's pane layout changed, including pane creation or removal.
    pub(crate) layout_changed: bool,
    /// A workspace entered or left the session.
    pub(crate) workspace_membership_changed: bool,
    /// The workspace order changed.
    pub(crate) workspace_order_changed: bool,
}

impl EndpointEffects {
    pub(crate) const fn needs_render(self) -> bool {
        self.shell_projection_changed
            || self.pane_surface_changed
            || self.changes_immediate_pty_sources()
            || self.workspace_order_changed
    }

    /// Changes that can alter which pane surfaces a client presents or which
    /// workspace geometry it controls.
    pub(crate) const fn changes_immediate_pty_sources(self) -> bool {
        self.focus_changed || self.layout_changed || self.workspace_membership_changed
    }
}

/// A command's answer, the workspace it moves the requesting client to, and
/// the changes it actually committed.
#[derive(Debug, PartialEq)]
pub(crate) struct Handled {
    pub(crate) reply: EndpointReply,
    /// The navigation effect: only the requesting client moves, and only to
    /// this workspace. `None` moves nobody.
    pub(crate) navigate: Option<WorkspaceId>,
    pub(crate) effects: EndpointEffects,
}

/// A refused command and any changes it committed before discovering the
/// refusal. Most refusals carry no effects, but retaining effects here keeps a
/// later reply lookup from hiding an earlier successful mutation.
#[derive(Debug, PartialEq)]
pub(crate) struct HandlerError {
    pub(crate) error: EndpointError,
    pub(crate) effects: EndpointEffects,
}

impl std::fmt::Display for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}

impl std::error::Error for HandlerError {}

impl From<EndpointError> for HandlerError {
    fn from(error: EndpointError) -> Self {
        Self {
            error,
            effects: EndpointEffects::default(),
        }
    }
}

pub(crate) type HandlerResult = Result<Handled, HandlerError>;

impl Handled {
    /// An acknowledgement that moves nobody.
    pub(crate) fn done() -> HandlerResult {
        Self::reply(EndpointReply::Done)
    }

    /// `reply`, moving nobody.
    pub(crate) fn reply(reply: EndpointReply) -> HandlerResult {
        Ok(Self {
            reply,
            navigate: None,
            effects: EndpointEffects::default(),
        })
    }

    /// An acknowledgement that moves nobody, with the effects the handler
    /// actually committed.
    pub(crate) fn done_with_effects(effects: EndpointEffects) -> HandlerResult {
        Self::reply_with_effects(EndpointReply::Done, effects)
    }

    /// `reply`, moving nobody, with the effects the handler actually committed.
    pub(crate) fn reply_with_effects(
        reply: EndpointReply,
        effects: EndpointEffects,
    ) -> HandlerResult {
        Ok(Self {
            reply,
            navigate: None,
            effects,
        })
    }

    /// `reply`, moving the requester to `workspace_id`.
    pub(crate) fn navigating(reply: EndpointReply, workspace_id: WorkspaceId) -> HandlerResult {
        Ok(Self {
            reply,
            navigate: Some(workspace_id),
            effects: EndpointEffects::default(),
        })
    }

    /// `reply`, moving the requester to `workspace_id`, with the effects the
    /// handler actually committed.
    pub(crate) fn navigating_with_effects(
        reply: EndpointReply,
        workspace_id: WorkspaceId,
        effects: EndpointEffects,
    ) -> HandlerResult {
        Ok(Self {
            reply,
            navigate: Some(workspace_id),
            effects,
        })
    }
}

/// An app refusal with a message for the user.
pub(crate) fn rejected<T>(message: impl Into<String>) -> Result<T, HandlerError> {
    Err(HandlerError {
        error: EndpointError::Rejected(message.into()),
        effects: EndpointEffects::default(),
    })
}

pub(crate) fn endpoint_rejected<T>(message: impl Into<String>) -> Result<T, EndpointError> {
    Err(EndpointError::Rejected(message.into()))
}

pub(crate) fn rejected_with_effects(
    message: impl Into<String>,
    effects: EndpointEffects,
) -> HandlerResult {
    Err(HandlerError {
        error: EndpointError::Rejected(message.into()),
        effects,
    })
}

pub(crate) fn workspace_missing(workspace_id: &WorkspaceId) -> EndpointError {
    EndpointError::Rejected(format!("workspace {workspace_id} not found"))
}

pub(crate) fn pane_missing(pane_id: &PublicPaneId) -> EndpointError {
    EndpointError::Rejected(format!("pane {pane_id} not found"))
}

impl App {
    /// The index of the workspace a command names, or the refusal for a
    /// workspace that is gone.
    pub(super) fn endpoint_workspace(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<usize, EndpointError> {
        self.resolve_workspace_id(workspace_id)
            .ok_or_else(|| workspace_missing(workspace_id))
    }

    /// The workspace index and pane a command names, or the refusal for a pane
    /// that is gone.
    pub(super) fn endpoint_pane(
        &self,
        pane_id: &PublicPaneId,
    ) -> Result<(usize, PaneId), EndpointError> {
        self.resolve_pane_id(pane_id)
            .ok_or_else(|| pane_missing(pane_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_refusals_name_the_subject() {
        let workspace = WorkspaceId::from_number(9).expect("nonzero number");
        assert_eq!(
            workspace_missing(&workspace).to_string(),
            "workspace w9 not found"
        );
        let pane = PublicPaneId::new(&workspace, 2);
        assert_eq!(pane_missing(&pane).to_string(), "pane w9:p2 not found");
        assert!(matches!(
            rejected::<()>("no"),
            Err(error) if error.error == EndpointError::Rejected("no".into())
        ));
    }
}
