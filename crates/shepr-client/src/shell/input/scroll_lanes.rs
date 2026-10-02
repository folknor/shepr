use std::collections::HashMap;

use shepr_protocol::{PublicPaneId, RequestId};
/// Per-pane scroll requests: one in flight, the newest offset queued behind it, and the
/// target offset until a surface shows it.
#[derive(Default)]
pub(in crate::shell) struct ScrollLanes(HashMap<PublicPaneId, ScrollLane>);
#[derive(Default)]
struct ScrollLane {
    /// The offset the user last asked for, until a surface shows it.
    target: Option<usize>,
    flight: Option<ScrollFlight>,
}
struct ScrollFlight {
    request: RequestId,
    /// Queued work exists only inside the flight it waits behind.
    queued: Option<usize>,
}
pub(in crate::shell) enum ScrollWant {
    Send,
    Queued,
}
#[derive(Debug, PartialEq, Eq)]
pub(in crate::shell) enum ScrollAnswer {
    Stale,
    Next(Option<usize>),
}
impl ScrollLanes {
    /// Records `offset` as the target; queues it (latest wins) behind a flight.
    pub(in crate::shell) fn want(&mut self, pane: &PublicPaneId, offset: usize) -> ScrollWant {
        let lane = self.0.entry(pane.clone()).or_default();
        lane.target = Some(offset);
        if let Some(flight) = lane.flight.as_mut() {
            flight.queued = Some(offset);
            ScrollWant::Queued
        } else {
            ScrollWant::Send
        }
    }
    /// Every dispatch, a queued one included, records its offset as the target, so a
    /// surface still showing the confirmed in-between offset does not clear it.
    pub(in crate::shell) fn sent(&mut self, pane: PublicPaneId, request: RequestId, offset: usize) {
        self.0.insert(
            pane,
            ScrollLane {
                target: Some(offset),
                flight: Some(ScrollFlight {
                    request,
                    queued: None,
                }),
            },
        );
    }
    pub(in crate::shell) fn send_failed(&mut self, pane: &PublicPaneId) {
        self.0.remove(pane);
    }
    /// Do not resurrect a target a surface already showed, or replace a queued target
    /// with the intermediate offset confirmed by this answer.
    pub(in crate::shell) fn answered(
        &mut self,
        pane: &PublicPaneId,
        request: &RequestId,
        confirmed: Option<usize>,
    ) -> ScrollAnswer {
        let Some(lane) = self.0.get_mut(pane) else {
            return ScrollAnswer::Stale;
        };
        if lane.flight.as_ref().is_none_or(|f| &f.request != request) {
            return ScrollAnswer::Stale;
        }
        let queued = lane.flight.take().and_then(|f| f.queued);
        if queued.is_none() && lane.target.is_some() && confirmed.is_some() {
            lane.target = confirmed;
        }
        if lane.target.is_none() {
            self.0.remove(pane);
        }
        ScrollAnswer::Next(queued)
    }
    /// Removes the whole lane (target, flight and queued offset) when `request` is its
    /// flight. Returns whether it was.
    pub(in crate::shell) fn failed(&mut self, pane: &PublicPaneId, request: &RequestId) -> bool {
        if self
            .0
            .get(pane)
            .and_then(|l| l.flight.as_ref())
            .is_some_and(|f| &f.request == request)
        {
            self.0.remove(pane);
            true
        } else {
            false
        }
    }
    pub(in crate::shell) fn target(&self, pane: &PublicPaneId) -> Option<usize> {
        self.0.get(pane).and_then(|l| l.target)
    }
    /// A surface shows `offset` for `pane`: a target clamped to `max` that it shows is
    /// done, and an empty lane goes.
    pub(in crate::shell) fn shown(&mut self, pane: &PublicPaneId, offset: usize, max: usize) {
        let Some(lane) = self.0.get_mut(pane) else {
            return;
        };
        if lane.target.is_some_and(|target| offset == target.min(max)) {
            lane.target = None;
        }
        if lane.target.is_none() && lane.flight.is_none() {
            self.0.remove(pane);
        }
    }
    /// Drops lanes of panes missing from a new snapshot, flights included; their ledger
    /// entries stay until answered, and the answer is then stale.
    pub(in crate::shell) fn retain_panes(&mut self, mut exists: impl FnMut(&PublicPaneId) -> bool) {
        self.0.retain(|id, _| exists(id));
    }
    pub(in crate::shell) fn clear(&mut self) {
        self.0.clear();
    }
}
#[cfg(test)]
impl ScrollLanes {
    pub(in crate::shell) fn queued(&self, pane: &PublicPaneId) -> Option<usize> {
        self.0
            .get(pane)
            .and_then(|l| l.flight.as_ref())
            .and_then(|f| f.queued)
    }
    pub(in crate::shell) fn in_flight(&self, pane: &PublicPaneId) -> bool {
        self.0.get(pane).is_some_and(|l| l.flight.is_some())
    }
    pub(in crate::shell) fn is_idle(&self) -> bool {
        self.0.is_empty()
    }
}
#[cfg(test)]
mod tests {
    use super::PublicPaneId;
    use crate::shell::input::scroll_lanes::{ScrollAnswer, ScrollLanes, ScrollWant};
    fn pane() -> PublicPaneId {
        crate::tests::test_pane_id("w1:p1")
    }
    fn flying() -> ScrollLanes {
        let mut s = ScrollLanes::default();
        s.sent(pane(), "first".into(), 3);
        s
    }
    #[test]
    fn a_queued_offset_exists_only_inside_a_flight() {
        let mut s = ScrollLanes::default();
        assert!(matches!(s.want(&pane(), 3), ScrollWant::Send));
        assert!(s.queued(&pane()).is_none());
        s.sent(pane(), "first".into(), 3);
        s.want(&pane(), 7);
        s.failed(&pane(), &"first".into());
        assert!(s.is_idle());
    }
    #[test]
    fn want_while_in_flight_queues_the_latest_offset() {
        let mut s = flying();
        s.want(&pane(), 7);
        s.want(&pane(), 9);
        assert_eq!(s.queued(&pane()), Some(9));
    }
    #[test]
    fn an_answer_for_another_request_is_stale() {
        let mut s = flying();
        assert_eq!(
            s.answered(&pane(), &"other".into(), Some(8)),
            ScrollAnswer::Stale
        );
        assert!(s.in_flight(&pane()));
    }
    #[test]
    fn an_answer_does_not_bring_back_a_target_a_surface_already_showed() {
        let mut s = flying();
        s.shown(&pane(), 3, 10);
        assert_eq!(
            s.answered(&pane(), &"first".into(), Some(3)),
            ScrollAnswer::Next(None)
        );
        assert!(s.is_idle());
    }
    #[test]
    fn a_dispatched_queued_offset_is_the_target_until_a_surface_shows_it() {
        let mut s = flying();
        s.want(&pane(), 7);
        assert_eq!(
            s.answered(&pane(), &"first".into(), Some(3)),
            ScrollAnswer::Next(Some(7))
        );
        s.sent(pane(), "second".into(), 7);
        s.shown(&pane(), 3, 10);
        assert_eq!(s.target(&pane()), Some(7));
    }
    #[test]
    fn a_failure_removes_target_and_queue_together() {
        let mut s = flying();
        s.want(&pane(), 7);
        assert!(s.failed(&pane(), &"first".into()));
        assert!(s.is_idle());
    }
    #[test]
    fn a_surface_showing_the_target_removes_it_and_an_empty_lane() {
        let mut s = flying();
        s.answered(&pane(), &"first".into(), Some(3));
        s.shown(&pane(), 3, 10);
        assert!(s.is_idle());
    }
}
