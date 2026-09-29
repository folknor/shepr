use crate::limits::MAX_EVENT_HISTORY;

#[derive(Clone, Default)]
pub struct EventHub {
    inner: std::sync::Arc<std::sync::Mutex<EventHubState>>,
    // The mutex can be poisoned after an event has been committed. Keep its
    // last published cursor separately so callers never restart from zero.
    current_sequence: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Default)]
struct EventHubState {
    next_sequence: u64,
    events: Vec<(u64, crate::schema::EventEnvelope)>,
}

/// Why the retained event history cannot answer a read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventHistoryError {
    /// Events after the requested sequence were dropped from the history.
    Lost,
    /// The history lock was poisoned.
    Unavailable,
}

impl EventHub {
    pub fn push(&self, event: crate::schema::EventEnvelope) {
        let sequence = {
            // A poisoned history may have inconsistent sequence and event data; do not publish
            // into it. Readers report the unavailable history instead of treating it as empty.
            let Ok(mut state) = self.inner.lock() else {
                return;
            };
            state.next_sequence += 1;
            let sequence = state.next_sequence;
            state.events.push((sequence, event));
            let overflow = state.events.len().saturating_sub(MAX_EVENT_HISTORY);
            if overflow > 0 {
                state.events.drain(0..overflow);
            }
            sequence
        };
        self.current_sequence
            .fetch_max(sequence, std::sync::atomic::Ordering::Release);
    }

    /// Every retained event after `sequence`, or why the history cannot say.
    pub fn events_after_checked(
        &self,
        sequence: u64,
    ) -> Result<Vec<(u64, crate::schema::EventEnvelope)>, EventHistoryError> {
        let state = self
            .inner
            .lock()
            .map_err(|_| EventHistoryError::Unavailable)?;
        if state
            .events
            .first()
            .is_some_and(|(first, _)| sequence < first.saturating_sub(1))
        {
            return Err(EventHistoryError::Lost);
        }
        Ok(state
            .events
            .iter()
            .filter(|(event_sequence, _)| *event_sequence > sequence)
            .cloned()
            .collect())
    }

    pub fn current_sequence(&self) -> u64 {
        self.current_sequence
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{EventData, EventEnvelope};

    fn event() -> EventEnvelope {
        EventEnvelope {
            data: EventData::WorkspaceFocused {
                workspace_id: "workspace_1".into(),
            },
        }
    }

    #[test]
    fn checked_history_distinguishes_retained_boundary_from_lost_events() {
        let hub = EventHub::default();
        assert!(
            hub.events_after_checked(0)
                .expect("test precondition")
                .is_empty()
        );
        for _ in 0..MAX_EVENT_HISTORY {
            hub.push(event());
        }
        assert_eq!(
            hub.events_after_checked(0)
                .expect("test precondition")
                .len(),
            MAX_EVENT_HISTORY
        );
        hub.push(event());
        assert_eq!(hub.events_after_checked(0), Err(EventHistoryError::Lost));
        let retained = hub.events_after_checked(1).expect("test precondition");
        assert_eq!(retained.len(), MAX_EVENT_HISTORY);
        assert_eq!(retained.first().expect("test precondition").0, 2);
        assert_eq!(
            retained.last().expect("test precondition").0,
            hub.current_sequence()
        );
        assert!(
            hub.events_after_checked(hub.current_sequence())
                .expect("test precondition")
                .is_empty()
        );
    }

    #[test]
    fn checked_history_reports_unavailable_instead_of_empty_after_poison() {
        let hub = EventHub::default();
        // A thread that panics while holding the lock poisons it; its join
        // reports the panic, so no catch_unwind is needed.
        let poisoner = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let _guard = hub.inner.lock().expect("test precondition");
                    panic!("poison the test event history");
                })
                .join()
        });
        assert!(poisoner.is_err());
        assert_eq!(
            hub.events_after_checked(0),
            Err(EventHistoryError::Unavailable)
        );
    }
}
