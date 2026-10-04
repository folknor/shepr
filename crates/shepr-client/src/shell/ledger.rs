//! Request ownership. Work can carry typed text; log ids and kinds only.

use crate::shell::copy::CopySession;
use crate::shell::copy::keys::drop_copy_flight;
use crate::shell::input::scroll_lanes::ScrollLanes;
use crate::shell::input::selection::MouseSelection;
use crate::shell::navigation::workspace_navigation::PendingWorkspaceHighlight;
use crate::shell::notices::{ClientEndpointNoticeKind, NoticeCode};
use crate::shell::overlays::{Overlay, drop_label_lookup};
use crate::shell::state::ClientShellAction;
use crate::shell::state::{
    ClientShellEndpointError, ClientShellInput, ClientShellState, Repaint, TypedText,
};
use crate::shell::{EndpointNotice, EndpointNoticeKind};
use shepr_protocol::command::{
    CommandKind, EndpointCommand, EndpointError, EndpointReply, PaneCopyMotionReply,
    PaneCopySearchReply, PaneInfoReply, PaneSelectionReply, WorkspaceCheckoutRootReply,
};
use shepr_protocol::{BootId, RequestId};
use std::collections::HashMap;
use std::time::Instant;

fn decode_reply<T: TryFrom<EndpointReply, Error = EndpointError>>(
    result: Result<EndpointReply, ClientShellEndpointError>,
) -> Result<T, ClientShellEndpointError> {
    result.and_then(|reply| T::try_from(reply).map_err(Into::into))
}

/// The token a continuation is checked against. A feature holds the ticket of the
/// answer it waits for, and that answer's `Work` carries the same ticket back. One
/// counter issues tickets for the life of the shell and does not wrap in practice, so a
/// ticket held by a value that was dropped and rebuilt can never match an answer meant
/// for the value it replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::shell) struct Ticket(u64);

/// One command bound for the active endpoint, with the id its answer comes back
/// under.
pub(crate) struct ClientShellEndpointRequest {
    pub(crate) id: RequestId,
    pub(crate) command: EndpointCommand,
}

impl std::fmt::Debug for ClientShellEndpointRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientShellEndpointRequest")
            .field("id", &self.id)
            .field("command", &"[redacted]")
            .finish()
    }
}

/// Owns shell request work. Only answer and drop paths take entries.
pub(in crate::shell) struct Ledger {
    next_ticket: u64,
    entries: HashMap<RequestId, Entry>,
}
impl Default for Ledger {
    fn default() -> Self {
        Self {
            next_ticket: 1,
            entries: HashMap::new(),
        }
    }
}
pub(in crate::shell) struct Entry {
    pub(in crate::shell) boot_id: BootId,
    pub(in crate::shell) command: CommandKind,
    pub(in crate::shell) work: Work,
}
impl Ledger {
    /// A ticket no other feature or earlier request holds.
    pub(in crate::shell) fn ticket(&mut self) -> Ticket {
        let ticket = Ticket(self.next_ticket);
        // A u64 counter of user requests does not run out in practice.
        self.next_ticket = self.next_ticket.saturating_add(1);
        ticket
    }
    /// Allocates a request identity and records its shell work.
    pub(in crate::shell) fn open(
        &mut self,
        boot_id: BootId,
        command: CommandKind,
        work: Work,
    ) -> RequestId {
        let id = RequestId::allocate();
        self.entries.insert(
            id.clone(),
            Entry {
                boot_id,
                command,
                work,
            },
        );
        id
    }
    pub(in crate::shell) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub(in crate::shell) fn ids(&self) -> Vec<RequestId> {
        self.entries.keys().cloned().collect()
    }
    fn take(&mut self, id: &RequestId) -> Option<Entry> {
        self.entries.remove(id)
    }
}
/// What a request owns: the feature state its answer completes and its drop rolls back.
#[derive(Debug)]
pub(in crate::shell) enum Work {
    /// A command whose answer needs no shell state.
    Plain,
    /// A focus command. A workspace highlight installed for it holds `highlight`.
    Focus {
        highlight: Ticket,
    },
    SelectionCopy,
    WorkspaceLabel {
        lookup: Ticket,
    },
    PaneScroll {
        pane_id: shepr_protocol::PublicPaneId,
        flight: Ticket,
    },
    WordSelection {
        pane_id: shepr_protocol::PublicPaneId,
        row: shepr_term::AbsRow,
        read: Ticket,
    },
    CopyMotion {
        pane_id: shepr_protocol::PublicPaneId,
        origin: shepr_protocol::command::PaneTextPoint,
        flight: Ticket,
    },
    CopySearch {
        pane_id: shepr_protocol::PublicPaneId,
        origin: shepr_protocol::command::PaneTextPoint,
        query: TypedText,
        direction: shepr_protocol::command::PaneCopySearchDirection,
        repeat: bool,
        flight: Ticket,
        rows: Ticket,
    },
}

/// Everything a request's drop may restore, as disjoint borrows of the shell's feature
/// state. It holds no path to the ledger or to `ClientShellState`, so a drop cannot open
/// a request.
pub(in crate::shell) struct Rollback<'a> {
    copy: &'a mut Option<CopySession>,
    mouse_selection: &'a mut MouseSelection,
    scroll_lanes: &'a mut ScrollLanes,
    overlay: &'a mut Option<Overlay>,
    highlight: &'a mut Option<PendingWorkspaceHighlight>,
}

/// Whether `submit` opened a ledger entry. A refused submit holds nothing, so the
/// caller has no ticket to revoke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::shell) enum Submitted {
    Opened,
    Refused,
}

/// Why a request leaves the ledger without an answer.
#[derive(Clone, Copy)]
pub(crate) enum DropReason {
    /// The request may have reached the server and its outcome is unknown: the
    /// connection was lost, or the lane cancelled it as possibly sent. Only work that
    /// reports interruption shows the interruption notice.
    Interrupted,
    /// Rejected before entering the send lane, so its outcome is known.
    Unsent,
    /// The answer came for another server boot than the request was made for.
    WrongBoot,
    /// Boot change or endpoint switch, with no interruption notice.
    Reset,
}
impl Work {
    fn accepts_reply(&self, reply: &EndpointReply) -> bool {
        match self {
            Self::Plain | Self::Focus { .. } => true,
            Self::SelectionCopy | Self::WordSelection { .. } => {
                matches!(reply, EndpointReply::PaneSelection { .. })
            }
            Self::WorkspaceLabel { .. } => {
                matches!(reply, EndpointReply::WorkspaceCheckoutRoot { .. })
            }
            Self::PaneScroll { .. } => matches!(reply, EndpointReply::PaneInfo { .. }),
            Self::CopyMotion { .. } => matches!(reply, EndpointReply::PaneCopyMotion { .. }),
            Self::CopySearch { .. } => matches!(reply, EndpointReply::PaneCopySearch { .. }),
        }
    }
    /// Whether losing it unanswered shows the interruption notice: commands that change
    /// server state. Reads only supply presentation.
    fn reports_interruption(&self) -> bool {
        matches!(self, Self::Plain | Self::Focus { .. })
    }
    /// The request ends without an answer. Restores exactly the state this request owns
    /// and returns a repaint decision. `parts` holds no path to the ledger or to
    /// `ClientShellState`, and there is no `outcome` sink, so a drop can neither open a
    /// request nor dispatch the queued work its caller may no longer have a connection for.
    fn dropped(self, parts: &mut Rollback<'_>) -> Repaint {
        match self {
            Self::Plain | Self::SelectionCopy => Repaint::Needed,
            Self::Focus { highlight } => {
                PendingWorkspaceHighlight::release(parts.highlight, highlight);
                Repaint::Needed
            }
            Self::WorkspaceLabel { lookup } => drop_label_lookup(parts.overlay, lookup),
            Self::PaneScroll { pane_id, flight } => {
                if parts.scroll_lanes.failed(&pane_id, flight) {
                    Repaint::Needed
                } else {
                    Repaint::Unchanged
                }
            }
            Self::WordSelection { read, .. } => parts.mouse_selection.drop_word_read(read),
            Self::CopyMotion { flight, .. } | Self::CopySearch { flight, .. } => {
                drop_copy_flight(parts.copy, flight)
            }
        }
    }
    /// The request was answered (a reply, a timeout or a server error). May queue input,
    /// actions and requests on `outcome`.
    fn answered(
        self,
        shell: &mut ClientShellState,
        result: Result<EndpointReply, ClientShellEndpointError>,
        now: Instant,
        outcome: &mut ClientShellInput,
    ) {
        let repaint = match self {
            // Close confirmation is client-owned (`open_confirm_close_overlay` runs before
            // the close is sent); endpoints close without asking back.
            Self::Plain => {
                if result.is_err() {
                    Repaint::Needed
                } else {
                    Repaint::Unchanged
                }
            }
            Self::Focus { highlight } => {
                if result.is_err() {
                    PendingWorkspaceHighlight::release(
                        &mut shell.pending_workspace_highlight,
                        highlight,
                    );
                    Repaint::Needed
                } else {
                    Repaint::Unchanged
                }
            }
            Self::WorkspaceLabel { lookup } => shell.complete_workspace_label_lookup(
                lookup,
                decode_reply::<WorkspaceCheckoutRootReply>(result).ok(),
            ),
            Self::PaneScroll { pane_id, flight } => shell.answer_pane_scroll(
                flight,
                &pane_id,
                decode_reply::<PaneInfoReply>(result),
                outcome,
            ),
            Self::SelectionCopy => match decode_reply::<PaneSelectionReply>(result) {
                Ok(PaneSelectionReply { text, .. }) if !text.is_empty() => {
                    outcome
                        .actions
                        .push(ClientShellAction::ClipboardWrite(text.into_bytes()));
                    Repaint::Unchanged
                }
                Ok(PaneSelectionReply { .. }) => {
                    if shell.push_endpoint_notice(
                        ClientEndpointNoticeKind::Rejected,
                        NoticeCode::SelectionEmpty,
                        "Nothing copied",
                        "The selection contained no text.",
                    ) {
                        Repaint::Needed
                    } else {
                        Repaint::Unchanged
                    }
                }
                Err(_) => Repaint::Needed,
            },
            Self::WordSelection { pane_id, row, read } => shell.complete_word_selection_row(
                read,
                &pane_id,
                row,
                decode_reply::<PaneSelectionReply>(result),
                now,
                outcome,
            ),
            Self::CopyMotion {
                pane_id,
                origin,
                flight,
            } => shell.complete_copy_motion(
                flight,
                &pane_id,
                origin,
                &decode_reply::<PaneCopyMotionReply>(result),
                outcome,
            ),
            Self::CopySearch {
                pane_id,
                origin,
                query,
                direction,
                repeat,
                flight,
                rows,
            } => shell.complete_copy_search(
                flight,
                rows,
                &pane_id,
                origin,
                query,
                direction,
                repeat,
                decode_reply::<PaneCopySearchReply>(result),
                outcome,
            ),
        };
        repaint.apply_to(outcome);
    }
}

impl ClientShellState {
    /// Opens a ledger entry for `command` at the current snapshot's boot and appends the
    /// endpoint action. `Refused` (and no entry) when the endpoint is not online or has no
    /// snapshot.
    pub(in crate::shell) fn submit(
        &mut self,
        command: EndpointCommand,
        work: Work,
        outcome: &mut ClientShellInput,
    ) -> Submitted {
        if command.traits().changes_focus {
            outcome.repaint |= self.pending_workspace_highlight.take().is_some();
        }
        if !self.endpoint_usable(self.endpoints.presented()) {
            let endpoint = self.endpoints.presented().clone();
            outcome.repaint |= self.receive_endpoint_unavailable(&EndpointNotice::new(
                endpoint,
                EndpointNoticeKind::NotReady,
            ));
            return Submitted::Refused;
        }
        let command_kind = command.kind();
        let Some(snapshot) = self.endpoints.active.snapshot() else {
            return Submitted::Refused;
        };
        let request_id = self
            .ledger
            .open(snapshot.boot_id.clone(), command_kind, work);
        outcome.actions.push(ClientShellAction::Endpoint {
            endpoint_id: self.endpoints.presented().clone(),
            boot_id: snapshot.boot_id.clone(),
            request: Box::new(ClientShellEndpointRequest {
                id: request_id,
                command,
            }),
        });
        Submitted::Opened
    }

    pub(in crate::shell) fn push_endpoint_command(
        &mut self,
        command: EndpointCommand,
        outcome: &mut ClientShellInput,
    ) {
        self.submit(command, Work::Plain, outcome);
    }
    /// Notices report the server answer even if a feature no longer awaits it.
    pub(crate) fn answer_request(
        &mut self,
        boot_id: &shepr_protocol::BootId,
        request_id: &RequestId,
        result: Result<EndpointReply, ClientShellEndpointError>,
        now: Instant,
    ) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        let Some(entry) = self.ledger.take(request_id) else {
            return outcome;
        };
        if entry.boot_id != *boot_id
            || self
                .endpoints
                .active
                .snapshot()
                .is_none_or(|s| s.boot_id != *boot_id)
        {
            self.dropped_entry(entry, DropReason::WrongBoot)
                .apply_to(&mut outcome);
            return outcome;
        }
        let result = result.and_then(|reply| {
            if entry.command.accepts_reply(&reply) && entry.work.accepts_reply(&reply) {
                Ok(reply)
            } else {
                Err(EndpointError::Internal(format!(
                    "endpoint returned an unexpected result for {}",
                    entry.command.name()
                ))
                .into())
            }
        });
        if result.is_ok() {
            self.notices
                .command_succeeded(&entry.boot_id, entry.command);
        }
        if let Err(error) = &result {
            let message = error.to_string();
            let (kind, code, title, body) = match error {
                ClientShellEndpointError::Timeout => (
                    ClientEndpointNoticeKind::Timeout,
                    NoticeCode::Command(entry.command),
                    "Server timed out",
                    format!("This server did not respond to {}.", entry.command.name()),
                ),
                ClientShellEndpointError::Server(EndpointError::ShuttingDown) => (
                    ClientEndpointNoticeKind::Unavailable,
                    NoticeCode::Server,
                    "Server unavailable",
                    message,
                ),
                ClientShellEndpointError::Server(_) => (
                    ClientEndpointNoticeKind::Rejected,
                    NoticeCode::Command(entry.command),
                    "Action rejected",
                    message,
                ),
            };
            outcome.repaint |=
                self.push_endpoint_notice_at_boot(Some(entry.boot_id), kind, code, title, body);
        }
        entry.work.answered(self, result, now, &mut outcome);
        outcome
    }
    /// Disjoint borrows of the feature state a drop restores.
    fn rollback(&mut self) -> Rollback<'_> {
        Rollback {
            copy: &mut self.copy,
            mouse_selection: &mut self.mouse_selection,
            scroll_lanes: &mut self.scroll_lanes,
            overlay: &mut self.overlay,
            highlight: &mut self.pending_workspace_highlight,
        }
    }
    fn dropped_entry(&mut self, entry: Entry, reason: DropReason) -> Repaint {
        let mut repaint = Repaint::Unchanged;
        if matches!(reason, DropReason::Interrupted)
            && entry.work.reports_interruption()
            && self.push_endpoint_notice_at_boot(
                Some(entry.boot_id),
                ClientEndpointNoticeKind::Unavailable,
                NoticeCode::Cancelled,
                "Action interrupted",
                "This server action was interrupted. Check its state before retrying.",
            )
        {
            repaint |= Repaint::Needed;
        }
        repaint |= entry.work.dropped(&mut self.rollback());
        repaint
    }
    /// Ends a request without an answer: shows the interruption notice where `reason`
    /// calls for it and runs its rollback (a focus request's releases its highlight). Returns a repaint decision.
    pub(crate) fn drop_request(&mut self, request_id: &RequestId, reason: DropReason) -> Repaint {
        let Some(entry) = self.ledger.take(request_id) else {
            return Repaint::Unchanged;
        };
        self.dropped_entry(entry, reason)
    }
    pub(in crate::shell) fn drop_all_requests(&mut self, reason: DropReason) -> Repaint {
        if self.ledger.is_empty() {
            return Repaint::Unchanged;
        }
        let mut repaint = Repaint::Unchanged;
        for id in self.ledger.ids() {
            repaint |= self.drop_request(&id, reason);
        }
        repaint
    }
}

#[cfg(test)]
impl Ticket {
    /// For tests that build feature state by hand. Counts down from the top of the
    /// range, which a ledger never reaches.
    pub(in crate::shell) const fn fixture(n: u64) -> Self {
        Self(u64::MAX - n)
    }
}

#[cfg(test)]
impl ClientShellState {
    /// Applies an endpoint response and returns everything it produced.
    ///
    /// A copy-mode motion or search response replays the keys queued while it
    /// was in flight, and those keys can yield pane input, a resize, a detach or
    /// host queries, not just repaints and actions. The caller must route the
    /// whole outcome (`finish_client_shell_input`), or the replayed keystrokes
    /// are lost.
    pub(in crate::shell) fn handle_endpoint_result(
        &mut self,
        boot_id: &shepr_protocol::BootId,
        request_id: &RequestId,
        result: Result<EndpointReply, ClientShellEndpointError>,
    ) -> ClientShellInput {
        // clock-io-ok: this test-only wrapper stands in for the client loop.
        self.answer_request(boot_id, request_id, result, std::time::Instant::now())
    }
}

#[cfg(test)]
impl Ledger {
    /// The one removal outside answer and drop, for tests that keep a single request.
    pub(in crate::shell) fn retain(&mut self, mut keep: impl FnMut(&RequestId) -> bool) {
        self.entries.retain(|id, _| keep(id));
    }
    pub(in crate::shell) fn contains(&self, id: &RequestId) -> bool {
        self.entries.contains_key(id)
    }
    pub(in crate::shell) fn len(&self) -> usize {
        self.entries.len()
    }
}
#[cfg(test)]
impl ClientShellState {
    /// Whether the ledger holds `id`. For `crate::tests`, which cannot see the
    /// `pub(in crate::shell)` `ledger` field.
    pub(crate) fn has_request(&self, id: &RequestId) -> bool {
        self.ledger.contains(id)
    }
}
#[cfg(test)]
mod tests {
    use crate::shell::ledger::{Ledger, Work};
    use shepr_protocol::command::CommandKind;

    #[test]
    fn ids_are_unique_and_never_reused() {
        let mut l = Ledger::default();
        let boot = crate::tests::test_boot_id("boot");
        let a = l.open(boot.clone(), CommandKind::WorkspaceRename, Work::Plain);
        l.take(&a);
        let b = l.open(boot, CommandKind::WorkspaceRename, Work::Plain);
        assert_ne!(a, b);
    }
    #[test]
    fn tickets_are_never_reissued() {
        let mut l = Ledger::default();
        let tickets: Vec<_> = (0..1000).map(|_| l.ticket()).collect();
        assert!(tickets.windows(2).all(|pair| pair[0].0 < pair[1].0));
    }
    #[test]
    fn an_entry_is_taken_once() {
        let mut l = Ledger::default();
        let id = l.open(
            crate::tests::test_boot_id("boot"),
            CommandKind::WorkspaceRename,
            Work::Plain,
        );
        assert!(l.take(&id).is_some());
        assert!(l.take(&id).is_none());
    }
}
