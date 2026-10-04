use super::MoveFailure;
use crate::shell::LocationTarget;
use shepr_protocol::{
    BootId, ClientMessage, RequestId,
    command::{EndpointCommand, EndpointReply, PaneTarget, WorkspaceTarget},
};
/// The navigation a move must show on its first frame: the desired target (`Machine`: none),
/// at most one request in flight, newer picks only replacing the desired target.
#[derive(Debug)]
pub(super) struct FocusLane {
    pub(super) desired: LocationTarget,
    in_flight: Option<(RequestId, LocationTarget)>,
    acknowledged: Option<LocationTarget>,
}
impl FocusLane {
    pub(super) fn new(desired: LocationTarget) -> Self {
        Self {
            desired,
            in_flight: None,
            acknowledged: None,
        }
    }
    pub(super) fn settled(&self) -> bool {
        self.in_flight.is_none()
            && (self.desired == LocationTarget::Machine || Some(self.desired) == self.acknowledged)
    }
    pub(super) fn accepts(&self, id: &RequestId) -> bool {
        self.in_flight
            .as_ref()
            .is_some_and(|(request, _)| request == id)
    }
    pub(super) fn request(&mut self, boot_id: &BootId) -> Option<ClientMessage> {
        if self.in_flight.is_some() || self.settled() {
            return None;
        }
        let target = self.desired;
        let command = match &target {
            LocationTarget::Pane(pane_id) => {
                EndpointCommand::PaneFocus(PaneTarget { pane_id: *pane_id })
            }
            LocationTarget::Workspace(workspace_id) => {
                EndpointCommand::WorkspaceFocus(WorkspaceTarget {
                    workspace_id: *workspace_id,
                })
            }
            LocationTarget::Machine => return None,
        };
        let request_id = RequestId::allocate();
        self.in_flight = Some((request_id.clone(), target));
        Some(ClientMessage::ClientShellEndpointRequest {
            boot_id: boot_id.clone(),
            request_id,
            command,
        })
    }
    pub(super) fn receive(&mut self, result: &EndpointReply) -> Result<(), MoveFailure> {
        let Some((_, requested)) = self.in_flight.take() else {
            return Err(MoveFailure::UnexpectedFocusResponse);
        };
        // The reply acknowledges the resolved target; Preparing checks actual focus against
        // the coherent snapshot and surface pair before committing the move.
        let matches = match (&requested, result) {
            (LocationTarget::Pane(id), EndpointReply::PaneInfo { pane }) => &pane.pane_id == id,
            (LocationTarget::Workspace(id), EndpointReply::WorkspaceInfo { workspace }) => {
                &workspace.workspace_id == id
            }
            _ => false,
        };
        if !matches {
            return Err(MoveFailure::BadFocusAcknowledgement);
        }
        self.acknowledged = Some(requested);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target(id: &str) -> LocationTarget {
        LocationTarget::Workspace(crate::tests::test_workspace_id(id))
    }
    fn reply(id: &str) -> EndpointReply {
        EndpointReply::WorkspaceInfo {
            workspace: shepr_protocol::command::WorkspaceInfo {
                workspace_id: crate::tests::test_workspace_id(id),
                label: id.into(),
                pane_count: 1,
                agent_status: shepr_protocol::AgentStatus::Idle,
            },
        }
    }
    fn request(lane: &mut FocusLane) -> Option<ClientMessage> {
        lane.request(&crate::tests::test_boot_id("boot"))
    }
    #[test]
    fn a_newer_focus_pick_replaces_the_desired_target_without_joining_the_request() {
        let mut lane = FocusLane::new(target("w1"));
        let first = request(&mut lane).expect("first focus");
        lane.desired = target("w2");
        assert!(request(&mut lane).is_none());
        lane.receive(&reply("w1")).expect("first reply");
        assert!(!lane.settled());
        let next = request(&mut lane).expect("latest focus");
        assert_ne!(first, next);
        assert!(
            matches!(next, ClientMessage::ClientShellEndpointRequest { command: EndpointCommand::WorkspaceFocus(WorkspaceTarget { workspace_id }), .. } if workspace_id == crate::tests::test_workspace_id("w2"))
        );
        lane.receive(&reply("w2")).expect("latest reply");
        assert!(lane.settled());
    }
    #[test]
    fn a_focus_request_is_built_once_until_its_response() {
        let mut lane = FocusLane::new(target("w1"));
        assert!(request(&mut lane).is_some());
        assert!(request(&mut lane).is_none());
        lane.receive(&reply("w1")).expect("focus reply");
        assert!(request(&mut lane).is_none());
    }
    #[test]
    fn a_focus_response_for_another_target_is_rejected() {
        let mut lane = FocusLane::new(target("w1"));
        request(&mut lane);
        assert!(lane.receive(&reply("w2")).is_err());
        assert!(!lane.settled());
    }
    #[test]
    fn removing_navigation_waits_for_the_in_flight_reply_then_settles() {
        let mut lane = FocusLane::new(target("w1"));
        request(&mut lane).expect("focus request");
        lane.desired = LocationTarget::Machine;
        assert!(!lane.settled());
        lane.receive(&reply("w1")).expect("focus reply");
        assert!(lane.settled());
        assert!(request(&mut lane).is_none());
    }
}
