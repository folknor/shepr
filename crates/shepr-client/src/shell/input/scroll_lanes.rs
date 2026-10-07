use std::collections::HashMap;

use crate::shell::ledger::Ticket;
use shepr_protocol::PublicPaneId;
/// Per-pane scroll requests: one in flight, the newest offset queued behind it, and the
/// target absolute viewport top until a surface shows it.
#[derive(Default)]
pub(in crate::shell) struct ScrollLanes(HashMap<PublicPaneId, ScrollLane>);
#[derive(Default)]
struct ScrollLane {
    /// The offset the user last asked for, until a surface shows it.
    target: Option<usize>,
    /// Absolute identity survives output moving the bottom between answer and surface.
    top: Option<shepr_term::AbsRow>,
    flight: Option<ScrollFlight>,
}
struct ScrollFlight {
    ticket: Ticket,
    /// Queued work exists only inside the flight it waits behind.
    queued: Option<usize>,
}
pub(super) enum ScrollWant {
    Send,
    Queued,
}
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ScrollAnswer {
    Stale,
    Next(Option<usize>),
}
impl ScrollLanes {
    /// Records `offset` as the target; queues it (latest wins) behind a flight.
    pub(super) fn want(&mut self, pane: &PublicPaneId, offset: usize) -> ScrollWant {
        let lane = self.0.entry(*pane).or_default();
        lane.target = Some(offset);
        lane.top = None;
        if let Some(flight) = lane.flight.as_mut() {
            flight.queued = Some(offset);
            ScrollWant::Queued
        } else {
            ScrollWant::Send
        }
    }
    /// Every dispatch, a queued one included, records its offset as the target, so a
    /// surface still showing the confirmed in-between offset does not clear it.
    pub(super) fn sent(&mut self, pane: PublicPaneId, flight: Ticket, offset: usize) {
        let top = self.0.get(&pane).and_then(|lane| lane.top);
        self.0.insert(
            pane,
            ScrollLane {
                target: Some(offset),
                top,
                flight: Some(ScrollFlight {
                    ticket: flight,
                    queued: None,
                }),
            },
        );
    }
    pub(super) fn send_failed(&mut self, pane: &PublicPaneId) {
        self.0.remove(pane);
    }
    /// Do not resurrect a target a surface already showed, or replace a queued target
    /// with the intermediate offset confirmed by this answer.
    pub(super) fn answered(
        &mut self,
        pane: &PublicPaneId,
        flight: Ticket,
        confirmed: Option<shepr_term::ScrollMetrics>,
    ) -> ScrollAnswer {
        let Some(lane) = self.0.get_mut(pane) else {
            return ScrollAnswer::Stale;
        };
        if lane.flight.as_ref().is_none_or(|f| f.ticket != flight) {
            return ScrollAnswer::Stale;
        }
        let queued = lane.flight.take().and_then(|f| f.queued);
        if queued.is_none()
            && lane.target.is_some()
            && let Some(confirmed) = confirmed
        {
            lane.target = Some(confirmed.offset_from_bottom);
            lane.top = (confirmed.offset_from_bottom != 0).then(|| confirmed.viewport_top_row());
        }
        if lane.target.is_none() && queued.is_none() {
            self.0.remove(pane);
        }
        ScrollAnswer::Next(queued)
    }
    /// Removes the whole lane (target, flight and queued offset) when `flight` is its
    /// flight. Returns whether it was.
    pub(in crate::shell) fn failed(&mut self, pane: &PublicPaneId, flight: Ticket) -> bool {
        if self
            .0
            .get(pane)
            .and_then(|l| l.flight.as_ref())
            .is_some_and(|f| f.ticket == flight)
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
    /// Records the requested top from the client's current row coordinates. Zero
    /// offset deliberately follows live output rather than pinning an absolute row.
    /// Out-of-range requests need the server's clamp before they identify a row.
    pub(in crate::shell) fn requested(
        &mut self,
        pane: &PublicPaneId,
        metrics: shepr_term::ScrollMetrics,
    ) {
        if let Some(lane) = self.0.get_mut(pane) {
            lane.top = lane
                .target
                .filter(|offset| *offset != 0 && *offset <= metrics.max_offset_from_bottom)
                .map(|offset| metrics.with_offset(offset).viewport_top_row());
        }
    }
    /// The absolute requested top, clamped to retained history when it is shown.
    /// An unanswered live-bottom request has no absolute top.
    pub(in crate::shell) fn top(&self, pane: &PublicPaneId) -> Option<shepr_term::AbsRow> {
        self.0.get(pane).and_then(|lane| lane.top)
    }
    /// A surface confirms an absolute top even if output increased its bottom offset.
    pub(in crate::shell) fn shown(
        &mut self,
        pane: &PublicPaneId,
        metrics: shepr_term::ScrollMetrics,
    ) {
        let Some(lane) = self.0.get_mut(pane) else {
            return;
        };
        let shown = lane.top.map_or_else(
            || {
                lane.target.is_some_and(|target| {
                    metrics.offset_from_bottom == target.min(metrics.max_offset_from_bottom)
                })
            },
            |top| {
                metrics.viewport_top_row()
                    == top.clamp(
                        metrics.history_origin,
                        metrics.with_offset(0).viewport_top_row(),
                    )
            },
        );
        if shown {
            lane.target = None;
            // A queued dispatch still needs its absolute destination, even when
            // another surface happened to show it before the flight answered.
            if lane
                .flight
                .as_ref()
                .is_none_or(|flight| flight.queued.is_none())
            {
                lane.top = None;
            }
        }
        if lane.target.is_none() && lane.flight.is_none() {
            self.0.remove(pane);
        }
    }
    /// Drops lanes of panes missing from a new snapshot, flights included. An answer for
    /// a dropped flight finds no lane holding its ticket and is stale.
    pub(in crate::shell) fn retain_panes(&mut self, mut exists: impl FnMut(&PublicPaneId) -> bool) {
        self.0.retain(|id, _| exists(id));
    }
    pub(in crate::shell) fn clear(&mut self) {
        self.0.clear();
    }
}
#[cfg(test)]
impl ScrollLanes {
    pub(in crate::shell) fn pending(
        pane: PublicPaneId,
        metrics: shepr_term::ScrollMetrics,
    ) -> Self {
        let mut lanes = Self::default();
        lanes.want(&pane, metrics.offset_from_bottom);
        lanes.requested(&pane, metrics);
        lanes.sent(pane, Ticket::fixture(2), metrics.offset_from_bottom);
        lanes
    }
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
    use crate::shell::ledger::Ticket;
    fn metrics(offset: usize, max: usize) -> shepr_term::ScrollMetrics {
        shepr_term::ScrollMetrics::new(offset, max, 5, shepr_term::AbsRow(0))
    }
    fn pane() -> PublicPaneId {
        crate::tests::test_pane_id("w1:p1")
    }
    fn flying() -> ScrollLanes {
        let mut s = ScrollLanes::default();
        s.sent(pane(), Ticket::fixture(1), 3);
        s
    }
    #[test]
    fn a_queued_offset_exists_only_inside_a_flight() {
        let mut s = ScrollLanes::default();
        assert!(matches!(s.want(&pane(), 3), ScrollWant::Send));
        assert!(s.queued(&pane()).is_none());
        s.sent(pane(), Ticket::fixture(1), 3);
        s.want(&pane(), 7);
        s.failed(&pane(), Ticket::fixture(1));
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
            s.answered(&pane(), Ticket::fixture(3), Some(metrics(8, 10))),
            ScrollAnswer::Stale
        );
        assert!(s.in_flight(&pane()));
    }
    #[test]
    fn an_answer_does_not_bring_back_a_target_a_surface_already_showed() {
        let mut s = flying();
        s.shown(&pane(), metrics(3, 10));
        assert_eq!(
            s.answered(&pane(), Ticket::fixture(1), Some(metrics(3, 10))),
            ScrollAnswer::Next(None)
        );
        assert!(s.is_idle());
    }
    #[test]
    fn a_dispatched_queued_offset_is_the_target_until_a_surface_shows_it() {
        let mut s = flying();
        s.want(&pane(), 7);
        assert_eq!(
            s.answered(&pane(), Ticket::fixture(1), Some(metrics(3, 10))),
            ScrollAnswer::Next(Some(7))
        );
        s.sent(pane(), Ticket::fixture(2), 7);
        s.shown(&pane(), metrics(3, 10));
        assert_eq!(s.target(&pane()), Some(7));
    }
    #[test]
    fn a_failure_removes_target_and_queue_together() {
        let mut s = flying();
        s.want(&pane(), 7);
        assert!(s.failed(&pane(), Ticket::fixture(1)));
        assert!(s.is_idle());
    }
    #[test]
    fn a_surface_showing_the_target_removes_it_and_an_empty_lane() {
        let mut s = flying();
        s.answered(&pane(), Ticket::fixture(1), Some(metrics(3, 10)));
        s.shown(&pane(), metrics(3, 10));
        assert!(s.is_idle());
    }
    #[test]
    fn output_between_answer_and_surface_does_not_strand_the_target() {
        let mut s = flying();
        s.answered(&pane(), Ticket::fixture(1), Some(metrics(3, 10)));
        assert_eq!(s.top(&pane()), Some(shepr_term::AbsRow(7)));
        // Four more lines move the bottom, but the viewport still starts on row 7.
        s.shown(&pane(), metrics(7, 14));
        assert!(s.is_idle());
    }

    #[test]
    fn output_before_the_answer_can_show_the_absolute_requested_top() {
        let mut s = ScrollLanes::default();
        s.want(&pane(), 3);
        s.requested(&pane(), metrics(0, 10));
        s.sent(pane(), Ticket::fixture(1), 3);
        s.shown(&pane(), metrics(7, 14));
        assert!(s.target(&pane()).is_none());
        s.answered(&pane(), Ticket::fixture(1), Some(metrics(3, 10)));
        assert!(s.is_idle());
    }

    #[test]
    fn an_evicted_target_completes_at_the_oldest_retained_row() {
        let mut s = flying();
        s.answered(&pane(), Ticket::fixture(1), Some(metrics(3, 10)));
        s.shown(
            &pane(),
            shepr_term::ScrollMetrics::new(10, 10, 5, shepr_term::AbsRow(9)),
        );
        assert!(s.is_idle());
    }

    #[test]
    fn live_bottom_targets_follow_output_instead_of_pinning_a_row() {
        let mut s = ScrollLanes::default();
        s.want(&pane(), 0);
        s.requested(&pane(), metrics(0, 10));
        s.sent(pane(), Ticket::fixture(1), 0);
        s.answered(&pane(), Ticket::fixture(1), Some(metrics(0, 12)));
        s.shown(&pane(), metrics(0, 14));
        assert!(s.is_idle());
    }

    #[test]
    fn a_queued_destination_survives_being_shown_before_its_dispatch() {
        let mut s = flying();
        s.want(&pane(), 7);
        s.requested(&pane(), metrics(0, 10));
        // This target is already visible before the prior request answers.
        s.shown(&pane(), metrics(7, 10));
        assert!(s.target(&pane()).is_none());
        assert_eq!(
            s.answered(&pane(), Ticket::fixture(1), Some(metrics(7, 14))),
            ScrollAnswer::Next(Some(7)),
        );
        assert_eq!(s.top(&pane()), Some(shepr_term::AbsRow(3)));
        // Dispatch rebases row 3 to offset 11 in the answer's newer history.
        s.sent(pane(), Ticket::fixture(2), 11);
        s.shown(&pane(), metrics(7, 14));
        assert_eq!(s.target(&pane()), Some(11));
        s.answered(&pane(), Ticket::fixture(2), Some(metrics(11, 14)));
        s.shown(&pane(), metrics(11, 14));
        assert!(s.is_idle());
    }
}
