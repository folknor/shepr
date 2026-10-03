use super::ClientEndpointId;
use crate::shell::ClientEndpointFocusTarget;
use shepr_protocol::{RequestId, TerminalGeometry};
use std::time::Instant;
mod focus_lane;
mod preparing;
pub use preparing::*;

/// The live or stale endpoint presentation, and the move toward the one the client wants.
/// The shell and transport routing both derive their endpoint identity from this owner. Plain data: no method takes the registry or
/// sends anything; the I/O lives in `endpoint::view` and the client loop's reconcile.
#[derive(Debug)]
pub enum EndpointChoice {
    /// The endpoint is selected and on screen.
    Showing(ClientEndpointId),
    /// `to` is selected and not on screen yet.
    Moving(Move),
}

/// A move from the endpoint on screen (if any) to the selected one.
#[derive(Debug)]
pub struct Move {
    /// The one presentation retained until the move commits, including after a loss.
    from: Presentation,
    to: ClientEndpointId,
    stage: MoveStage,
}

#[derive(Debug)]
enum Presentation {
    Live(ClientEndpointId),
    Stale(ClientEndpointId),
}

impl Presentation {
    fn endpoint(&self) -> &ClientEndpointId {
        match self {
            Self::Live(endpoint) | Self::Stale(endpoint) => endpoint,
        }
    }

    fn live(&self) -> Option<&ClientEndpointId> {
        match self {
            Self::Live(endpoint) => Some(endpoint),
            Self::Stale(_) => None,
        }
    }
}

#[derive(Debug)]
pub enum MoveStage {
    /// Nothing sent yet: `to` has no connection with metadata for its current generation.
    /// `focus` is the navigation the user asked for, handed to the focus lane when preparing
    /// starts.
    Waiting {
        focus: Option<ClientEndpointFocusTarget>,
    },
    /// `to` has been turned on and the client is collecting a coherent pair.
    Preparing(Box<Preparing>),
    /// Only with nothing shown: preparing `to` failed on connection generation `generation`.
    /// Not retried until `to` has a connection of another generation with metadata, or the
    /// user selects again. With a shown `from` a failure returns to showing it instead.
    Failed { generation: u64 },
}

/// How an inbound message from one connection relates to the choice. `Target` only while the
/// move to that endpoint is `Preparing`: a `Waiting` or `Failed` target has nothing to collect
/// evidence into, so it is `Other`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionRole {
    Shown,
    Target,
    Other,
}

/// What a shell pick did to the choice.
#[derive(Debug, PartialEq, Eq)]
pub enum Selection {
    /// Already shown, nothing to move, nothing to navigate.
    Unchanged,
    /// Already shown, and the pick carried navigation: the caller applies it through the
    /// ordinary endpoint-command path.
    FocusShown(ClientEndpointFocusTarget),
    /// The choice is now (or still) a move; the reconcile drives it.
    Moving,
}

/// What losing a connection did to the choice.
#[derive(Debug, PartialEq, Eq)]
pub enum Lost {
    Shown,
    Target,
    Unrelated,
}

/// A move that may start: `Waiting`, or `Failed` (then `failed_generation` is set and a start
/// needs a connection of another generation).
pub struct PendingStart<'a> {
    pub to: &'a ClientEndpointId,
    pub from: Option<&'a ClientEndpointId>,
    pub failed_generation: Option<u64>,
}

pub struct FailedMove {
    pub to: ClientEndpointId,
    pub returned_to: Option<ClientEndpointId>,
}

pub struct Committed {
    pub previous: Option<ClientEndpointId>,
    pub shown: ClientEndpointId,
}

impl EndpointChoice {
    /// Launch with Local connected.
    pub fn showing(endpoint: ClientEndpointId) -> Self {
        Self::Showing(endpoint)
    }

    /// Launch with Local unreachable: nothing shown, waiting for `to`.
    pub fn waiting_for(to: ClientEndpointId) -> Self {
        Self::Moving(Move {
            from: Presentation::Stale(to.clone()),
            to,
            stage: MoveStage::Waiting { focus: None },
        })
    }
    /// The endpoint whose live or stale presentation the client draws.
    pub fn presented(&self) -> &ClientEndpointId {
        match self {
            Self::Showing(endpoint) => endpoint,
            Self::Moving(movement) => movement.from.endpoint(),
        }
    }

    /// The live endpoint eligible for input and inbound host effects.
    pub fn live(&self) -> Option<&ClientEndpointId> {
        match self {
            Self::Showing(e) => Some(e),
            Self::Moving(m) => m.from.live(),
        }
    }

    pub fn role(&self, endpoint: &ClientEndpointId) -> ConnectionRole {
        if self.live() == Some(endpoint) {
            ConnectionRole::Shown
        } else if self
            .preparing()
            .is_some_and(|p| &p.lease().endpoint_id == endpoint)
        {
            ConnectionRole::Target
        } else {
            ConnectionRole::Other
        }
    }
    /// The shown endpoint and the target being prepared; every other viewed connection is
    /// turned off by the reconcile.
    pub fn wants_view(&self, endpoint: &ClientEndpointId) -> bool {
        self.role(endpoint) != ConnectionRole::Other
    }

    /// Applies a shell pick. Selecting the shown endpoint cancels any move; selecting the
    /// target again only replaces its navigation (and rearms a failed move); anything else
    /// starts a new move from the shown endpoint.
    pub fn select(
        &mut self,
        endpoint: ClientEndpointId,
        focus: Option<ClientEndpointFocusTarget>,
    ) -> Selection {
        if self.live() == Some(&endpoint) {
            *self = Self::Showing(endpoint);
            return focus.map_or(Selection::Unchanged, Selection::FocusShown);
        }
        if let Self::Moving(m) = self
            && m.to == endpoint
        {
            match &mut m.stage {
                MoveStage::Preparing(p) => p.retarget_focus(focus),
                _ => m.stage = MoveStage::Waiting { focus },
            }
        } else {
            *self = Self::Moving(Move {
                from: if self.live().is_some() {
                    Presentation::Live(self.presented().clone())
                } else {
                    Presentation::Stale(self.presented().clone())
                },
                to: endpoint,
                stage: MoveStage::Waiting { focus },
            });
        }
        Selection::Moving
    }

    /// Losing the shown connection leaves nothing shown and keeps the selection (a target
    /// being prepared keeps preparing). Losing the target returns to the shown endpoint, or
    /// waits for the target's next connection when nothing is shown.
    pub fn connection_lost(&mut self, endpoint: &ClientEndpointId) -> Lost {
        if self.live() == Some(endpoint) {
            match self {
                Self::Showing(e) => *self = Self::waiting_for(e.clone()),
                Self::Moving(m) => m.from = Presentation::Stale(m.from.endpoint().clone()),
            }
            Lost::Shown
        } else if let Self::Moving(m) = self
            && &m.to == endpoint
        {
            if let Some(from) = m.from.live() {
                *self = Self::Showing(from.clone());
            } else {
                m.stage = MoveStage::Waiting { focus: None };
            }
            Lost::Target
        } else {
            Lost::Unrelated
        }
    }

    pub fn pending_start(&self) -> Option<PendingStart<'_>> {
        let Self::Moving(m) = self else {
            return None;
        };
        let failed_generation = match m.stage {
            MoveStage::Waiting { .. } => None,
            MoveStage::Failed { generation } => Some(generation),
            MoveStage::Preparing(_) => return None,
        };
        Some(PendingStart {
            to: &m.to,
            from: m.from.live(),
            failed_generation,
        })
    }

    /// `Waiting` with a shown `from`: back to `Showing(from)`; returns `to`. Any other state:
    /// unchanged, `None`.
    pub fn abandon(&mut self) -> Option<ClientEndpointId> {
        if let Self::Moving(m) = self
            && matches!(m.stage, MoveStage::Waiting { .. })
            && let Some(from) = m.from.live().cloned()
        {
            let to = m.to.clone();
            *self = Self::Showing(from);
            Some(to)
        } else {
            None
        }
    }

    /// `Waiting` or `Failed`: becomes `Preparing`, taking the `Waiting` focus. Any other
    /// state: unchanged.
    pub fn begin_preparing(
        &mut self,
        lease: ViewLease,
        view_request: RequestId,
        geometry: TerminalGeometry,
        now: Instant,
    ) {
        if let Self::Moving(m) = self
            && !matches!(m.stage, MoveStage::Preparing(_))
        {
            let focus = match &mut m.stage {
                MoveStage::Waiting { focus } => focus.take(),
                _ => None,
            };
            m.stage = MoveStage::Preparing(Box::new(Preparing::new(
                lease,
                view_request,
                geometry,
                focus,
                now,
            )));
        }
    }
    pub fn preparing(&self) -> Option<&Preparing> {
        match self {
            Self::Moving(Move {
                stage: MoveStage::Preparing(p),
                ..
            }) => Some(p),
            _ => None,
        }
    }
    pub fn preparing_mut(&mut self) -> Option<&mut Preparing> {
        match self {
            Self::Moving(Move {
                stage: MoveStage::Preparing(p),
                ..
            }) => Some(p),
            _ => None,
        }
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.preparing().map(Preparing::deadline)
    }

    /// `Preparing` only: back to `Showing(from)` when `from` exists, else `Failed` on the
    /// lease's generation. Any other state: unchanged, `None`.
    pub fn fail_move(&mut self) -> Option<FailedMove> {
        let Self::Moving(m) = self else {
            return None;
        };
        let MoveStage::Preparing(p) = &m.stage else {
            return None;
        };
        let failed = FailedMove {
            to: m.to.clone(),
            returned_to: m.from.live().cloned(),
        };
        if let Some(from) = m.from.live() {
            *self = Self::Showing(from.clone());
        } else {
            m.stage = MoveStage::Failed {
                generation: p.lease().generation,
            };
        }
        Some(failed)
    }

    /// `Preparing` only: becomes `Showing(to)`. Any other state: unchanged, `None`.
    pub fn commit(&mut self) -> Option<Committed> {
        let Self::Moving(m) = self else {
            return None;
        };
        if !matches!(m.stage, MoveStage::Preparing(_)) {
            return None;
        }
        let committed = Committed {
            previous: m.from.live().cloned(),
            shown: m.to.clone(),
        };
        *self = Self::Showing(m.to.clone());
        Some(committed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn remote() -> ClientEndpointId {
        ClientEndpointId::Ssh(super::super::MachineLabel::parse("build").expect("machine"))
    }
    pub(super) fn geometry() -> TerminalGeometry {
        TerminalGeometry::new(80, 24, 8, 16, false)
    }
    pub(super) fn lease() -> ViewLease {
        ViewLease {
            endpoint_id: remote(),
            generation: 7,
            boot_id: crate::tests::test_boot_id("remote-boot"),
            minimum_revision: 1.into(),
        }
    }
    pub(super) fn preparing() -> EndpointChoice {
        let mut choice = EndpointChoice::showing(ClientEndpointId::Local);
        choice.select(remote(), None);
        choice.begin_preparing(
            lease(),
            "client-shell-view:1:on".into(),
            geometry(),
            Instant::now(),
        );
        choice
    }
    fn navigation() -> ClientEndpointFocusTarget {
        ClientEndpointFocusTarget::Workspace(crate::tests::test_workspace_id("w1"))
    }
    #[test]
    fn a_launch_with_local_connected_shows_local() {
        let c = EndpointChoice::showing(ClientEndpointId::Local);
        assert_eq!(c.live(), Some(&ClientEndpointId::Local));
    }
    #[test]
    fn an_unreachable_local_at_launch_waits_with_nothing_shown() {
        let c = EndpointChoice::waiting_for(ClientEndpointId::Local);
        assert_eq!(
            c.pending_start().expect("waiting").to,
            &ClientEndpointId::Local
        );
    }
    #[test]
    fn selecting_the_shown_endpoint_changes_nothing() {
        let mut c = EndpointChoice::showing(ClientEndpointId::Local);
        assert_eq!(
            c.select(ClientEndpointId::Local, None),
            Selection::Unchanged
        );
        assert!(c.pending_start().is_none());
    }
    #[test]
    fn selecting_the_shown_endpoint_with_navigation_asks_to_focus_it() {
        let mut c = EndpointChoice::showing(ClientEndpointId::Local);
        assert_eq!(
            c.select(ClientEndpointId::Local, Some(navigation())),
            Selection::FocusShown(navigation())
        );
    }
    #[test]
    fn selecting_another_endpoint_keeps_the_shown_one_until_commit() {
        let mut c = EndpointChoice::showing(ClientEndpointId::Local);
        c.select(remote(), None);
        assert_eq!(c.live(), Some(&ClientEndpointId::Local));
        assert_eq!(c.pending_start().expect("waiting").to, &remote());
    }
    #[test]
    fn a_newer_selection_replaces_the_target_and_only_the_new_one_is_wanted() {
        let mut c = preparing();
        let next = ClientEndpointId::Ssh(super::super::MachineLabel::parse("next").expect("label"));
        c.select(next.clone(), None);
        assert_eq!(c.pending_start().expect("waiting").to, &next);
        assert_eq!(c.live(), Some(&ClientEndpointId::Local));
        assert!(!c.wants_view(&remote()));
    }
    #[test]
    fn selecting_the_shown_endpoint_during_a_move_cancels_it() {
        let mut c = preparing();
        assert_eq!(
            c.select(ClientEndpointId::Local, None),
            Selection::Unchanged
        );
        assert!(c.preparing().is_none());
        assert!(!c.wants_view(&remote()));
    }
    #[test]
    fn begin_preparing_takes_the_waiting_focus() {
        let mut c = EndpointChoice::waiting_for(remote());
        c.select(remote(), Some(navigation()));
        c.begin_preparing(
            lease(),
            "client-shell-view:1:on".into(),
            geometry(),
            Instant::now(),
        );
        let request = c
            .preparing_mut()
            .expect("preparing")
            .focus_request()
            .expect("focus");
        assert!(matches!(
            request,
            shepr_protocol::ClientMessage::ClientShellEndpointRequest {
                command: shepr_protocol::command::EndpointCommand::WorkspaceFocus(_),
                ..
            }
        ));
    }
    #[test]
    fn a_commit_shows_the_target_and_ends_the_move() {
        let mut c = preparing();
        let committed = c.commit().expect("commit");
        assert_eq!(committed.previous, Some(ClientEndpointId::Local));
        assert_eq!(c.live(), Some(&remote()));
        assert!(c.preparing().is_none());
    }
    #[test]
    fn a_failed_move_with_a_shown_source_returns_to_showing_it() {
        let mut c = preparing();
        let failed = c.fail_move().expect("failed");
        assert_eq!(failed.returned_to, Some(ClientEndpointId::Local));
        assert_eq!(c.live(), Some(&ClientEndpointId::Local));
        assert!(c.pending_start().is_none());
    }
    #[test]
    fn a_failed_move_with_nothing_shown_is_not_retried_on_the_same_generation() {
        let mut c = preparing();
        c.connection_lost(&ClientEndpointId::Local);
        c.fail_move();
        let p = c.pending_start().expect("failed");
        assert_eq!(p.failed_generation, Some(7));
        assert!(c.live().is_none());
    }
    #[test]
    fn an_explicit_selection_rearms_a_failed_move() {
        let mut c = preparing();
        c.connection_lost(&ClientEndpointId::Local);
        c.fail_move();
        c.select(remote(), Some(navigation()));
        assert_eq!(c.pending_start().expect("waiting").failed_generation, None);
    }
    #[test]
    fn abandon_returns_to_the_shown_source_only_from_waiting() {
        let mut c = preparing();
        assert!(c.abandon().is_none());
        c.select(remote(), None);
        assert!(c.abandon().is_none());
        let mut c = EndpointChoice::waiting_for(remote());
        assert!(c.abandon().is_none());
        c = EndpointChoice::showing(ClientEndpointId::Local);
        c.select(remote(), None);
        assert_eq!(c.abandon(), Some(remote()));
        assert_eq!(c.live(), Some(&ClientEndpointId::Local));
    }
    #[test]
    fn commit_and_fail_move_do_nothing_outside_preparing() {
        for mut c in [
            EndpointChoice::showing(ClientEndpointId::Local),
            EndpointChoice::waiting_for(remote()),
        ] {
            let shown = c.live().cloned();
            assert!(c.commit().is_none());
            assert!(c.fail_move().is_none());
            assert_eq!(c.live(), shown.as_ref());
        }
    }
    #[test]
    fn losing_the_shown_connection_leaves_nothing_shown_and_keeps_the_selection() {
        let mut c = EndpointChoice::showing(remote());
        assert_eq!(c.connection_lost(&remote()), Lost::Shown);
        assert!(c.live().is_none());
        assert_eq!(c.presented(), &remote());
        assert_eq!(c.pending_start().expect("waiting").to, &remote());
    }
    #[test]
    fn losing_the_target_connection_returns_to_the_shown_endpoint() {
        let mut c = preparing();
        assert_eq!(c.connection_lost(&remote()), Lost::Target);
        assert_eq!(c.live(), Some(&ClientEndpointId::Local));
        assert!(c.preparing().is_none());
    }
    #[test]
    fn losing_the_shown_connection_keeps_a_healthy_target_preparing() {
        let mut c = preparing();
        assert_eq!(c.connection_lost(&ClientEndpointId::Local), Lost::Shown);
        assert!(c.live().is_none());
        assert_eq!(c.presented(), &ClientEndpointId::Local);
        assert!(c.preparing().is_some());
    }
    #[test]
    fn losing_both_connections_retains_the_source_presentation() {
        let mut c = preparing();
        assert_eq!(c.connection_lost(&ClientEndpointId::Local), Lost::Shown);
        assert_eq!(c.connection_lost(&remote()), Lost::Target);
        assert!(c.live().is_none());
        assert_eq!(c.presented(), &ClientEndpointId::Local);
        assert_eq!(c.pending_start().expect("waiting target").to, &remote());
    }
    #[test]
    fn losing_an_unrelated_connection_changes_nothing() {
        let mut c = EndpointChoice::showing(ClientEndpointId::Local);
        assert_eq!(c.connection_lost(&remote()), Lost::Unrelated);
        assert_eq!(c.live(), Some(&ClientEndpointId::Local));
    }
    #[test]
    fn wants_view_is_the_shown_endpoint_and_the_preparing_target() {
        let c = preparing();
        assert!(c.wants_view(&ClientEndpointId::Local));
        assert!(c.wants_view(&remote()));
    }
    #[test]
    fn role_is_target_only_while_preparing() {
        let mut c = EndpointChoice::waiting_for(remote());
        assert_eq!(c.role(&remote()), ConnectionRole::Other);
        c.begin_preparing(lease(), "request".into(), geometry(), Instant::now());
        assert_eq!(c.role(&remote()), ConnectionRole::Target);
        c.fail_move();
        assert_eq!(c.role(&remote()), ConnectionRole::Other);
    }
}
