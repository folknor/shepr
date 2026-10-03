//! Request ownership. Work can carry typed text; log ids and kinds only.

use crate::shell::overlays::notices::{ClientEndpointNoticeKind, NoticeCode};
use crate::shell::state::ClientShellAction;
use crate::shell::state::{
    ClientShellEndpointError, ClientShellEndpointRequest, ClientShellInput, ClientShellState,
    Repaint, TypedText,
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

/// Owns shell request work. Only answer and drop paths take entries.
pub(in crate::shell) struct Ledger {
    next: u64,
    entries: HashMap<RequestId, Entry>,
}
impl Default for Ledger {
    fn default() -> Self {
        Self {
            next: 1,
            entries: HashMap::new(),
        }
    }
}
pub(in crate::shell) struct Entry {
    issued_at: u64,
    pub(in crate::shell) boot_id: BootId,
    pub(in crate::shell) command: CommandKind,
    pub(in crate::shell) work: Work,
}
impl Ledger {
    /// Allocates a request identity and records its shell work.
    pub(in crate::shell) fn open(
        &mut self,
        boot_id: BootId,
        command: CommandKind,
        work: Work,
    ) -> RequestId {
        let id = RequestId::allocate();
        let issued_at = self.next;
        // A u64 counter of user requests does not run out in practice.
        self.next = self.next.saturating_add(1);
        self.entries.insert(
            id.clone(),
            Entry {
                issued_at,
                boot_id,
                command,
                work,
            },
        );
        id
    }
    pub(in crate::shell) fn work(&self, id: &RequestId) -> Option<&Work> {
        self.entries.get(id).map(|e| &e.work)
    }
    /// The serial the next `open` issues. Serials only grow, so comparing two marks
    /// tells whether anything was opened between them, even if it was removed since.
    pub(in crate::shell) fn mark(&self) -> u64 {
        self.next
    }
    /// The entries still held that were opened at or after `mark`, in issue order.
    fn opened_since(&self, mark: u64) -> Vec<RequestId> {
        let mut opened: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.issued_at >= mark)
            .map(|(id, entry)| (entry.issued_at, id.clone()))
            .collect();
        opened.sort_unstable_by_key(|(issued_at, _)| *issued_at);
        opened.into_iter().map(|(_, id)| id).collect()
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
    SelectionCopy,
    WorkspaceLabel,
    PaneScroll {
        pane_id: shepr_protocol::PublicPaneId,
    },
    WordSelection {
        pane_id: shepr_protocol::PublicPaneId,
        row: shepr_vt::AbsRow,
    },
    CopyMotion {
        pane_id: shepr_protocol::PublicPaneId,
        origin: shepr_protocol::command::PaneTextPoint,
    },
    CopySearch {
        pane_id: shepr_protocol::PublicPaneId,
        origin: shepr_protocol::command::PaneTextPoint,
        query: TypedText,
        direction: shepr_protocol::command::PaneCopySearchDirection,
        repeat: bool,
        /// The copy-mode search generation, not a request guard.
        generation: u64,
    },
}

/// Why a request leaves the ledger without an answer.
#[derive(Clone, Copy)]
pub(crate) enum DropReason {
    /// The request may have reached the server and its outcome is unknown: the
    /// connection was lost, the lane cancelled it as possibly sent, or its endpoint is no
    /// longer the active one when its answer or lane expiry arrives. Only a `Plain`
    /// request shows the interruption notice; read requests only supply presentation.
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
            Self::Plain => true,
            Self::SelectionCopy | Self::WordSelection { .. } => {
                matches!(reply, EndpointReply::PaneSelection { .. })
            }
            Self::WorkspaceLabel => matches!(reply, EndpointReply::WorkspaceCheckoutRoot { .. }),
            Self::PaneScroll { .. } => matches!(reply, EndpointReply::PaneInfo { .. }),
            Self::CopyMotion { .. } => matches!(reply, EndpointReply::PaneCopyMotion { .. }),
            Self::CopySearch { .. } => matches!(reply, EndpointReply::PaneCopySearch { .. }),
        }
    }
    /// The request ends without an answer. Restores exactly the state this request owns
    /// and returns a repaint decision. It has no `outcome` sink, so it cannot dispatch the
    /// queued work its caller may no longer have a connection for.
    fn dropped(self, shell: &mut ClientShellState, request: &RequestId) -> Repaint {
        match self {
            Self::Plain | Self::SelectionCopy => Repaint::Needed,
            Self::WorkspaceLabel => shell.complete_workspace_label_lookup(request, None),
            Self::PaneScroll { pane_id } => shell.drop_pane_scroll(request, &pane_id),
            Self::WordSelection { .. } => shell.drop_word_selection(request),
            Self::CopyMotion { .. } | Self::CopySearch { .. } => shell.drop_copy_operation(request),
        }
    }
    /// The request was answered (a reply, a timeout or a server error). May queue input,
    /// actions and requests on `outcome`.
    fn answered(
        self,
        shell: &mut ClientShellState,
        request: &RequestId,
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
            Self::WorkspaceLabel => shell.complete_workspace_label_lookup(
                request,
                decode_reply::<WorkspaceCheckoutRootReply>(result).ok(),
            ),
            Self::PaneScroll { pane_id } => shell.answer_pane_scroll(
                request,
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
            Self::WordSelection { pane_id, row } => shell.complete_word_selection_row(
                request,
                &pane_id,
                row,
                decode_reply::<PaneSelectionReply>(result),
                now,
                outcome,
            ),
            Self::CopyMotion { pane_id, origin } => shell.complete_copy_motion(
                request,
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
                generation,
            } => shell.complete_copy_search(
                request,
                &pane_id,
                origin,
                query,
                direction,
                repeat,
                generation,
                decode_reply::<PaneCopySearchReply>(result),
                outcome,
            ),
        };
        repaint.apply_to(outcome);
    }
}

impl ClientShellState {
    /// Opens a ledger entry for `command` at the current snapshot's boot and appends the
    /// endpoint action. `None` (and no entry) when the endpoint is not online or has no
    /// snapshot.
    pub(in crate::shell) fn submit(
        &mut self,
        command: EndpointCommand,
        work: Work,
        outcome: &mut ClientShellInput,
    ) -> Option<shepr_protocol::RequestId> {
        if command.traits().changes_focus {
            outcome.repaint |= self.pending_workspace_highlight.take().is_some();
        }
        if !self.endpoint_usable(self.endpoints.presented()) {
            let endpoint = self.endpoints.presented().clone();
            outcome.repaint |= self.receive_endpoint_unavailable(&EndpointNotice::new(
                endpoint,
                EndpointNoticeKind::NotReady,
            ));
            return None;
        }
        let command_kind = command.kind();
        let snapshot = self.snapshot.as_deref()?;
        let request_id = self
            .ledger
            .open(snapshot.boot_id.clone(), command_kind, work);
        outcome.actions.push(ClientShellAction::Endpoint {
            endpoint_id: self.endpoints.presented().clone(),
            boot_id: snapshot.boot_id.clone(),
            request: Box::new(ClientShellEndpointRequest {
                id: request_id.clone(),
                command,
            }),
        });
        Some(request_id)
    }

    pub(in crate::shell) fn push_endpoint_command(
        &mut self,
        command: EndpointCommand,
        outcome: &mut ClientShellInput,
    ) {
        self.submit(command, Work::Plain, outcome);
    }
    fn release_highlight(&mut self, request: &RequestId) -> Repaint {
        if self
            .pending_workspace_highlight
            .as_ref()
            .is_some_and(|h| &h.request_id == request)
        {
            self.pending_workspace_highlight = None;
            Repaint::Needed
        } else {
            Repaint::Unchanged
        }
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
        let request = request_id.clone();
        if entry.boot_id != *boot_id
            || self
                .snapshot
                .as_deref()
                .is_none_or(|s| s.boot_id != *boot_id)
        {
            self.dropped_entry(entry, &request, DropReason::WrongBoot)
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
            self.release_highlight(request_id).apply_to(&mut outcome);
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
        entry
            .work
            .answered(self, &request, result, now, &mut outcome);
        outcome
    }
    fn dropped_entry(&mut self, entry: Entry, request: &RequestId, reason: DropReason) -> Repaint {
        let mut repaint = self.release_highlight(request);
        if matches!(reason, DropReason::Interrupted)
            && matches!(entry.work, Work::Plain)
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
        let mark = self.ledger.mark();
        repaint |= entry.work.dropped(self, request);
        // A rollback that called `submit` with a throwaway input would open an orphan
        // entry whose action is lost; the types cannot rule that out. The rollback test
        // pins that no `Work` kind does (the ledger mark does not move across a drop).
        // Should one slip through, its action never left the shell, so it is dropped as
        // unsent instead of waiting forever for an answer.
        let orphans = self.ledger.opened_since(mark);
        if !orphans.is_empty() {
            tracing::error!(
                request = %request,
                orphans = orphans.len(),
                "request rollback opened requests; dropping them as unsent"
            );
            for orphan in orphans {
                repaint |= self.drop_request(&orphan, DropReason::Unsent);
            }
        }
        repaint
    }
    /// Ends a request without an answer: releases its highlight, shows the interruption
    /// notice where `reason` calls for it and runs its rollback. Returns a repaint decision.
    pub(crate) fn drop_request(&mut self, request_id: &RequestId, reason: DropReason) -> Repaint {
        let Some(entry) = self.ledger.take(request_id) else {
            return Repaint::Unchanged;
        };
        self.dropped_entry(entry, request_id, reason)
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
impl ClientShellState {
    /// Applies an endpoint response and returns everything it produced.
    ///
    /// A copy-mode motion or search response replays the keys queued while it
    /// was in flight, and those keys can yield pane input, a resize, a detach or
    /// host queries, not just repaints and actions. The caller must route the
    /// whole outcome (`finish_client_shell_input`), or the replayed keystrokes
    /// are lost.
    pub(crate) fn handle_endpoint_result(
        &mut self,
        boot_id: &shepr_protocol::BootId,
        request_id: &str,
        result: Result<EndpointReply, ClientShellEndpointError>,
    ) -> ClientShellInput {
        // clock-io-ok: this test-only wrapper stands in for the client loop.
        self.answer_request(
            boot_id,
            &request_id.into(),
            result,
            std::time::Instant::now(),
        )
    }
}

#[cfg(test)]
impl Ledger {
    /// The one removal outside answer and drop, for tests that keep a single request.
    pub(in crate::shell) fn retain(&mut self, mut keep: impl FnMut(&RequestId) -> bool) {
        self.entries.retain(|id, _| keep(id));
    }
    pub(in crate::shell) fn contains(&self, id: &str) -> bool {
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
    pub(crate) fn has_request(&self, id: &str) -> bool {
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
