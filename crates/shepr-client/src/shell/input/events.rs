use super::*;

/// Cached cost of the latest pane-input message built for one input outcome.
#[derive(Default)]
pub(super) struct PaneInputBatchAccounting {
    request_index: Option<usize>,
    expanded_events: usize,
    text_bytes: usize,
}

impl PaneInputBatchAccounting {
    fn record(&mut self, request_index: usize, expanded_events: usize, text_bytes: usize) {
        self.request_index = Some(request_index);
        self.expanded_events = expanded_events;
        self.text_bytes = text_bytes;
    }
}

pub(super) fn target_event_message(
    target: shepr_protocol::PublicPaneId,
    event: ClientPaneInputEvent,
) -> ClientMessage {
    ClientMessage::ClientShellPaneInput {
        pane_id: target,
        events: vec![event],
    }
}

/// Adds an event to the pending target message while both server limits allow it.
pub(super) fn push_target_event(
    target: shepr_protocol::PublicPaneId,
    event: ClientPaneInputEvent,
    outcome: &mut ClientShellInput,
    accounting: &mut PaneInputBatchAccounting,
) {
    let request_index = outcome.requests.len().checked_sub(1);
    let event_count = event.expanded_event_count();
    let event_bytes = event.text_bytes();
    if let Some(ClientMessage::ClientShellPaneInput {
        pane_id: pending_pane,
        events,
    }) = outcome.requests.last_mut()
        && *pending_pane == target
    {
        let cached = request_index.is_some_and(|index| accounting.request_index == Some(index));
        let (pending_count, pending_bytes) = if cached {
            (accounting.expanded_events, accounting.text_bytes)
        } else {
            events
                .iter()
                .fold((0usize, 0usize), |(count, bytes), event| {
                    (
                        count.saturating_add(event.expanded_event_count()),
                        bytes.saturating_add(event.text_bytes()),
                    )
                })
        };
        if pending_count.saturating_add(event_count) <= shepr_protocol::MAX_INPUT_EVENT_BATCH
            && pending_bytes.saturating_add(event_bytes) <= shepr_protocol::MAX_INPUT_PAYLOAD
        {
            events.push(event);
            if let Some(request_index) = request_index {
                accounting.record(
                    request_index,
                    pending_count.saturating_add(event_count),
                    pending_bytes.saturating_add(event_bytes),
                );
            }
            return;
        }
    }
    outcome.requests.push(target_event_message(target, event));
    if let Some(request_index) = outcome.requests.len().checked_sub(1) {
        accounting.record(request_index, event_count, event_bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_protocol::{MAX_INPUT_EVENT_BATCH, MAX_INPUT_PAYLOAD};

    fn batch(events: Vec<ClientPaneInputEvent>) -> Vec<ClientMessage> {
        let pane = crate::tests::test_pane_id("w1:p1");
        let mut outcome = ClientShellInput::default();
        let mut accounting = PaneInputBatchAccounting::default();
        for event in events {
            push_target_event(pane.clone(), event, &mut outcome, &mut accounting);
        }
        outcome.requests
    }

    /// The expanded count and text bytes of each message, as the server
    /// charges them.
    fn charges(messages: &[ClientMessage]) -> Vec<(usize, usize)> {
        messages
            .iter()
            .map(|message| {
                let ClientMessage::ClientShellPaneInput { events, .. } = message else {
                    panic!("expected pane input, got {message:?}");
                };
                events.iter().fold((0, 0), |(count, bytes), event| {
                    (
                        count + event.expanded_event_count(),
                        bytes + event.text_bytes(),
                    )
                })
            })
            .collect()
    }

    fn scroll(lines: u16) -> ClientPaneInputEvent {
        ClientPaneInputEvent::Mouse {
            kind: shepr_protocol::ClientMouseKind::ScrollUp,
            position: shepr_protocol::ClientMousePosition::Cell { column: 0, row: 0 },
            geometry: None,
            modifiers: shepr_protocol::WireModifiers::NONE,
            lines,
        }
    }

    #[test]
    fn text_joining_a_full_paste_starts_a_new_message() {
        let messages = batch(vec![
            ClientPaneInputEvent::Paste("p".repeat(MAX_INPUT_PAYLOAD)),
            ClientPaneInputEvent::TextCommit("x".into()),
        ]);
        assert_eq!(charges(&messages), [(1, MAX_INPUT_PAYLOAD), (1, 1)]);
    }

    #[test]
    fn a_batch_is_split_exactly_at_the_expanded_event_limit() {
        let events = (0..=MAX_INPUT_EVENT_BATCH)
            .map(|_| ClientPaneInputEvent::TextCommit(String::new()))
            .collect();
        assert_eq!(
            charges(&batch(events)),
            [(MAX_INPUT_EVENT_BATCH, 0), (1, 0)]
        );

        let max_lines = u16::try_from(MAX_INPUT_EVENT_BATCH).expect("the limit fits a scroll step");
        let messages = batch(vec![scroll(max_lines), scroll(1), scroll(1)]);
        assert_eq!(charges(&messages), [(MAX_INPUT_EVENT_BATCH, 0), (2, 0)]);
    }
}
