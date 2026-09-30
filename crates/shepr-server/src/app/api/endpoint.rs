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

/// A command's answer and the workspace it moves the requesting client to.
#[derive(Debug, PartialEq)]
pub(crate) struct Handled {
    pub(crate) reply: EndpointReply,
    /// The navigation effect: only the requesting client moves, and only to
    /// this workspace. `None` moves nobody.
    pub(crate) navigate: Option<WorkspaceId>,
}

pub(crate) type HandlerResult = Result<Handled, EndpointError>;

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
        })
    }

    /// `reply`, moving the requester to `workspace_id`.
    pub(crate) fn navigating(reply: EndpointReply, workspace_id: WorkspaceId) -> HandlerResult {
        Ok(Self {
            reply,
            navigate: Some(workspace_id),
        })
    }
}

/// An app refusal with a message for the user.
pub(crate) fn rejected<T>(message: impl Into<String>) -> Result<T, EndpointError> {
    Err(EndpointError::Rejected(message.into()))
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
            Err(EndpointError::Rejected(message)) if message == "no"
        ));
    }
}
