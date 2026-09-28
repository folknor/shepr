use super::*;

pub(super) fn target_event_message(
    target: shepr_protocol::PublicPaneId,
    event: ClientPaneInputEvent,
) -> ClientMessage {
    ClientMessage::ClientShellPaneInput {
        pane_id: target,
        events: vec![event],
    }
}

pub(super) fn push_target_event(
    target: shepr_protocol::PublicPaneId,
    event: ClientPaneInputEvent,
    outcome: &mut ClientShellInput,
) {
    if let Some(ClientMessage::ClientShellPaneInput {
        pane_id: pending_pane,
        events,
    }) = outcome.requests.last_mut()
        && *pending_pane == target
    {
        events.push(event);
        return;
    }
    outcome.requests.push(target_event_message(target, event));
}
