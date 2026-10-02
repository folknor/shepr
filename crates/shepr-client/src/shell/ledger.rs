//! Request ownership. Work can carry typed text; log ids and kinds only.
use super::*;
use shepr_protocol::command::{EndpointCommand, EndpointError, EndpointReply};
use shepr_protocol::{BootId, RequestId};
use std::time::Instant;

/// The sole owner of issued request identities. Only answer and drop paths take entries.
pub(super) struct Ledger {
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
pub(super) struct Entry {
    pub(super) boot_id: BootId,
    pub(super) method: String,
    pub(super) work: Work,
}
impl Ledger {
    /// Issues `client-shell:{n}` and records the entry.
    pub(super) fn open(&mut self, boot_id: BootId, method: String, work: Work) -> RequestId {
        let id = Self::id_for(self.next);
        // A u64 counter of user requests does not run out in practice.
        self.next = self.next.saturating_add(1);
        self.entries.insert(
            id.clone(),
            Entry {
                boot_id,
                method,
                work,
            },
        );
        id
    }
    pub(super) fn work(&self, id: &RequestId) -> Option<&Work> {
        self.entries.get(id).map(|e| &e.work)
    }
    fn id_for(serial: u64) -> RequestId {
        format!("client-shell:{serial}").into()
    }
    /// The serial the next `open` issues. Serials only grow, so comparing two marks
    /// tells whether anything was opened between them, even if it was removed since.
    pub(super) fn mark(&self) -> u64 {
        self.next
    }
    /// The entries still held that were opened at or after `mark`.
    fn opened_since(&self, mark: u64) -> Vec<RequestId> {
        (mark..self.next)
            .map(Self::id_for)
            .filter(|id| self.entries.contains_key(id))
            .collect()
    }
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub(super) fn ids(&self) -> Vec<RequestId> {
        self.entries.keys().cloned().collect()
    }
    fn take(&mut self, id: &str) -> Option<Entry> {
        self.entries.remove(id)
    }
}
/// What a request owns: the feature state its answer completes and its drop rolls back.
#[derive(Debug)]
pub(super) enum Work {
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
    /// The request ends without an answer. Restores exactly the state this request owns
    /// and returns whether to repaint. It has no `outcome` sink, so it cannot dispatch the
    /// queued work its caller may no longer have a connection for.
    fn dropped(self, shell: &mut ClientShellState, request: &RequestId) -> bool {
        match self {
            Self::Plain | Self::SelectionCopy => true,
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
            Self::Plain => result.is_err(),
            Self::WorkspaceLabel => shell.complete_workspace_label_lookup(request, result.ok()),
            Self::PaneScroll { pane_id } => {
                shell.answer_pane_scroll(request, &pane_id, result, now, outcome)
            }
            Self::SelectionCopy => match result {
                Ok(EndpointReply::PaneSelection { text, .. }) if !text.is_empty() => {
                    outcome
                        .actions
                        .push(ClientShellAction::ClipboardWrite(text.into_bytes()));
                    false
                }
                Ok(EndpointReply::PaneSelection { .. }) => shell.push_endpoint_notice(
                    ClientEndpointNoticeKind::Rejected,
                    "selection_empty",
                    "Nothing copied",
                    "The selection contained no text.",
                ),
                Ok(_) => {
                    shell.set_endpoint_error(
                        "endpoint returned an unexpected selection result",
                        now,
                    );
                    true
                }
                Err(_) => true,
            },
            Self::WordSelection { pane_id, row } => {
                shell.complete_word_selection_row(request, &pane_id, row, result, now, outcome)
            }
            Self::CopyMotion { pane_id, origin } => {
                shell.complete_copy_motion(request, &pane_id, origin, result, now, outcome)
            }
            Self::CopySearch {
                pane_id,
                origin,
                query,
                direction,
                repeat,
                generation,
            } => shell.complete_copy_search(
                request, &pane_id, origin, query, direction, repeat, generation, result, now,
                outcome,
            ),
        };
        outcome.repaint |= repaint;
    }
}
impl ClientShellState {
    /// Opens a ledger entry for `command` at the current snapshot's boot and appends the
    /// endpoint action. `None` (and no entry) when the endpoint is not online or has no
    /// snapshot.
    pub(super) fn submit(
        &mut self,
        command: EndpointCommand,
        work: Work,
        outcome: &mut ClientShellInput,
    ) -> Option<shepr_protocol::RequestId> {
        let changes_focus = matches!(
            &command,
            EndpointCommand::WorkspaceFocus(_)
                | EndpointCommand::PaneFocus(_)
                | EndpointCommand::PaneFocusDirection(_)
                | EndpointCommand::WorkspaceCreate(_)
                | EndpointCommand::PaneSplit(_)
        );
        if changes_focus {
            outcome.repaint |= self.pending_workspace_highlight.take().is_some();
        }
        if !self.endpoint_is_online(&self.active_endpoint_id) {
            let label = self.active_endpoint_label().to_owned();
            outcome.repaint |= self.receive_endpoint_unavailable(format!("{label} is not ready"));
            return None;
        }
        let method_name = command.name().to_owned();
        let snapshot = self.snapshot.as_deref()?;
        let request_id = self
            .ledger
            .open(snapshot.boot_id.clone(), method_name, work);
        outcome.actions.push(ClientShellAction::Endpoint {
            endpoint_id: self.active_endpoint_id.clone(),
            boot_id: snapshot.boot_id.clone(),
            request: Box::new(ClientShellEndpointRequest {
                id: request_id.to_string(),
                command,
            }),
        });
        Some(request_id)
    }

    pub(super) fn push_endpoint_command(
        &mut self,
        command: EndpointCommand,
        outcome: &mut ClientShellInput,
    ) {
        self.submit(command, Work::Plain, outcome);
    }
    fn release_highlight(&mut self, request: &str) -> bool {
        if self
            .pending_workspace_highlight
            .as_ref()
            .is_some_and(|h| h.request_id == request)
        {
            self.pending_workspace_highlight = None;
            true
        } else {
            false
        }
    }
    /// Notices report the server answer even if a feature no longer awaits it.
    pub(crate) fn answer_request(
        &mut self,
        boot_id: &str,
        request_id: &str,
        result: Result<EndpointReply, ClientShellEndpointError>,
        now: Instant,
    ) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        let Some(entry) = self.ledger.take(request_id) else {
            return outcome;
        };
        let request: RequestId = request_id.into();
        if entry.boot_id != boot_id
            || self
                .snapshot
                .as_deref()
                .is_none_or(|s| s.boot_id != boot_id)
        {
            outcome.repaint = self.dropped_entry(entry, &request, DropReason::WrongBoot);
            return outcome;
        }
        if result.is_ok() {
            self.endpoint_notice_seen.remove(&ClientEndpointNoticeKey {
                boot_id: Some(entry.boot_id.clone()),
                kind: ClientEndpointNoticeKind::Timeout,
                code: entry.method.clone(),
            });
        }
        if let Err(error) = &result {
            outcome.repaint |= self.release_highlight(request_id);
            let message = error.to_string();
            let (kind, code, title, body) = match error {
                ClientShellEndpointError::Timeout => (
                    ClientEndpointNoticeKind::Timeout,
                    entry.method.clone(),
                    "Server timed out",
                    format!("This server did not respond to {}.", entry.method),
                ),
                ClientShellEndpointError::Server(EndpointError::ShuttingDown) => (
                    ClientEndpointNoticeKind::Unavailable,
                    "server".to_owned(),
                    "Server unavailable",
                    message,
                ),
                ClientShellEndpointError::Server(_) => (
                    ClientEndpointNoticeKind::Rejected,
                    format!("{}:{message}", entry.method),
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
    fn dropped_entry(&mut self, entry: Entry, request: &RequestId, reason: DropReason) -> bool {
        let mut repaint = self.release_highlight(request);
        if matches!(reason, DropReason::Interrupted) && matches!(entry.work, Work::Plain) {
            repaint |= self.push_endpoint_notice_at_boot(
                Some(entry.boot_id),
                ClientEndpointNoticeKind::Unavailable,
                "cancelled",
                "Action interrupted",
                "This server action was interrupted. Check its state before retrying.",
            );
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
    /// notice where `reason` calls for it and runs its rollback. Returns whether to repaint.
    pub(crate) fn drop_request(&mut self, request_id: &str, reason: DropReason) -> bool {
        let Some(entry) = self.ledger.take(request_id) else {
            return false;
        };
        self.dropped_entry(entry, &request_id.into(), reason)
    }
    pub(super) fn drop_all_requests(&mut self, reason: DropReason) -> bool {
        if self.ledger.is_empty() {
            return false;
        }
        let mut repaint = false;
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
        boot_id: &str,
        request_id: &str,
        result: Result<EndpointReply, ClientShellEndpointError>,
    ) -> ClientShellInput {
        // clock-io-ok: this test-only wrapper stands in for the client loop.
        self.answer_request(boot_id, request_id, result, std::time::Instant::now())
    }
}

#[cfg(test)]
impl Ledger {
    /// The one removal outside answer and drop, for tests that keep a single request.
    pub(super) fn retain(&mut self, mut keep: impl FnMut(&RequestId) -> bool) {
        self.entries.retain(|id, _| keep(id));
    }
    pub(super) fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
}
#[cfg(test)]
impl ClientShellState {
    /// Whether the ledger holds `id`. For `crate::tests`, which cannot see the
    /// `pub(super)` `ledger` field.
    pub(crate) fn has_request(&self, id: &str) -> bool {
        self.ledger.contains(id)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ids_are_unique_and_never_reused() {
        let mut l = Ledger::default();
        let boot = crate::tests::test_boot_id("boot");
        let a = l.open(boot.clone(), "action".into(), Work::Plain);
        l.take(&a);
        let b = l.open(boot, "action".into(), Work::Plain);
        assert_ne!(a, b);
    }
    #[test]
    fn an_entry_is_taken_once() {
        let mut l = Ledger::default();
        let id = l.open(
            crate::tests::test_boot_id("boot"),
            "action".into(),
            Work::Plain,
        );
        assert!(l.take(&id).is_some());
        assert!(l.take(&id).is_none());
    }
}
