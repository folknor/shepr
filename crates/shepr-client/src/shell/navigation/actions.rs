use super::*;
use shepr_protocol::command::{EndpointCommand, EndpointError, EndpointReply};

impl PendingEndpointKind {
    /// Read requests only supply client presentation state, so losing their connection is not
    /// an interrupted user action with an unknown server-side outcome.
    fn should_show_cancelled_notice(&self) -> bool {
        matches!(self, Self::Generic)
    }

    /// Roll back only the state owned by this request. Cancellation cannot dispatch work:
    /// its caller may have lost presentation or the connection that would carry that work.
    fn cancel(self, shell: &mut ClientShellState) -> bool {
        match self {
            Self::Generic | Self::SelectionCopy => true,
            Self::WorkspaceLabel { lookup_id } => shell.complete_workspace_label_lookup(
                lookup_id,
                Err(ClientShellEndpointError::Cancelled),
            ),
            Self::PaneScroll { pane_id, serial } => {
                if shell.pane_scroll_in_flight.get(&pane_id).copied() != Some(serial) {
                    return false;
                }
                shell.pane_scroll_in_flight.remove(&pane_id);
                shell.pane_scroll_queued.remove(&pane_id);
                shell.pane_scroll_targets.remove(&pane_id);
                true
            }
            Self::WordSelection {
                pane_id,
                absolute_row,
                generation,
            } => shell.cancel_word_selection_row(&pane_id, absolute_row, generation),
            Self::CopyMotion {
                session_generation, ..
            }
            | Self::CopySearch {
                session_generation, ..
            } => {
                if shell.copy_session_generation != session_generation {
                    return false;
                }
                // Buffered keys depend on a result we will never apply. Discard them rather
                // than replaying exits, new motions, or pane input into a frozen presentation.
                shell.reset_copy_pipeline();
                if let Some(copy_mode) = shell.copy_mode.as_mut() {
                    copy_mode.copy_after_search = false;
                }
                true
            }
        }
    }
}

impl ClientShellState {
    pub(super) fn record_binding(
        &mut self,
        binding: &shepr_termio::input::KeybindMatch,
        outcome: &mut ClientShellInput,
    ) {
        match binding {
            shepr_termio::input::KeybindMatch::Action(
                shepr_termio::input::KeybindAction::Detach,
            ) => {
                outcome.detach = true;
            }
            shepr_termio::input::KeybindMatch::Action(
                shepr_termio::input::KeybindAction::ToggleSidebar,
            ) => {
                self.sidebar_collapsed = !self.sidebar_collapsed;
                self.sidebar_collapsed_manual = true;
                self.reveal_navigation_workspace = true;
                // The retained surface stays on screen, clipped to the new pane area, until the
                // endpoint answers the resize.
                outcome.repaint = true;
                outcome.resize = true;
                self.persist_chrome_preferences(outcome);
            }
            shepr_termio::input::KeybindMatch::Action(action) => {
                let action = *action;
                if self.workspace_preview_action_blocked()
                    && matches!(
                        action,
                        shepr_termio::input::KeybindAction::RenameWorkspace
                            | shepr_termio::input::KeybindAction::CloseWorkspace
                    )
                {
                    let open_workspace = self.open_workspace_hint();
                    self.receive_endpoint_unavailable(format!(
                        "Select an available workspace and {open_workspace} before renaming or closing it"
                    ));
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::OpenNavigator {
                    self.open_navigator_overlay();
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::Help {
                    self.overlay = Some(ClientShellOverlay::Help(ClientHelpOverlay {
                        query: TextEditor::default(),
                        search_focused: false,
                        scroll: 0,
                    }));
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::NewWorkspace {
                    if self.config.prompt_new_workspace_name {
                        self.open_new_workspace_overlay(outcome);
                    } else {
                        self.push_endpoint_command(
                            EndpointCommand::WorkspaceCreate(
                                shepr_protocol::command::WorkspaceCreateParams {
                                    source: match self.workspace_action_id() {
                                        Some(workspace_id) => {
                                            shepr_protocol::command::WorkspaceCreateSource::Follow(
                                                workspace_id,
                                            )
                                        }
                                        None => {
                                            shepr_protocol::command::WorkspaceCreateSource::Default
                                        }
                                    },
                                    label: None,
                                },
                            ),
                            outcome,
                        );
                    }
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::RenameWorkspace {
                    self.open_rename_workspace_overlay();
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::CloseWorkspace {
                    if let Some(workspace_id) = self.workspace_action_id() {
                        if self.config.confirm_close {
                            self.open_confirm_close_overlay(workspace_id);
                        } else {
                            self.push_endpoint_command(
                                EndpointCommand::WorkspaceClose(
                                    shepr_protocol::command::WorkspaceCloseParams { workspace_id },
                                ),
                                outcome,
                            );
                        }
                    }
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::RenamePane {
                    self.open_rename_pane_overlay();
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::WorkspacePicker {
                    self.pending_workspace_highlight = None;
                    self.mode = ClientShellMode::Navigate;
                    self.navigate_workspace_id = self.focused_navigation_target();
                    self.reveal_navigation_workspace = true;
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::EnterResizeMode {
                    self.mode = ClientShellMode::Resize;
                    outcome.repaint = true;
                    return;
                }
                if action == shepr_termio::input::KeybindAction::CopyMode {
                    if self.enter_copy_mode(outcome) {
                        outcome.repaint = true;
                    }
                    return;
                }
                if self.handle_endpoint_navigation(action, outcome) {
                    return;
                }
                if let Some(command) = self.endpoint_command_for_action(action) {
                    self.push_endpoint_command(command, outcome);
                }
            }
        }
    }

    /// Copies the current selection. The read is by absolute row against the
    /// live terminal, with no content revision: output between the displayed
    /// frame and this request must not reject the copy.
    pub(super) fn request_selection_copy(&mut self, outcome: &mut ClientShellInput) {
        let Some(selection) = self.selection.as_ref() else {
            return;
        };
        let pane_id = selection.pane_id.clone();
        let (anchor, cursor) = selection.ordered_cells();
        self.push_endpoint_command_with_kind(
            EndpointCommand::PaneSelectionRead(shepr_protocol::command::PaneSelectionReadParams {
                pane_id,
                anchor: shepr_protocol::command::PaneTextPoint {
                    row: anchor.0,
                    col: anchor.1,
                },
                cursor: shepr_protocol::command::PaneTextPoint {
                    row: cursor.0,
                    col: cursor.1,
                },
            }),
            PendingEndpointKind::SelectionCopy,
            outcome,
        );
    }

    pub(super) fn push_endpoint_command(
        &mut self,
        command: EndpointCommand,
        outcome: &mut ClientShellInput,
    ) {
        self.push_endpoint_command_with_kind(command, PendingEndpointKind::Generic, outcome);
    }

    pub(super) fn push_endpoint_notice(
        &mut self,
        kind: ClientEndpointNoticeKind,
        code: impl Into<String>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> bool {
        let boot_id = if kind == ClientEndpointNoticeKind::Unavailable {
            None
        } else {
            self.snapshot
                .as_deref()
                .map(|snapshot| snapshot.boot_id.clone())
        };
        self.push_endpoint_notice_at_boot(boot_id, kind, code, title, body)
    }

    fn push_endpoint_notice_at_boot(
        &mut self,
        boot_id: Option<shepr_protocol::BootId>,
        kind: ClientEndpointNoticeKind,
        code: impl Into<String>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> bool {
        let key = ClientEndpointNoticeKey {
            boot_id,
            kind,
            code: code.into(),
        };
        let body = body.into();
        // Only timeouts are suppressed until a later success. Availability and rejection notices
        // can recur after dismissal or expiry, while identical visible cards do not keep resetting
        // their lifetime.
        match kind {
            ClientEndpointNoticeKind::Rejected | ClientEndpointNoticeKind::Unavailable => {
                if self
                    .visible_endpoint_notice
                    .as_ref()
                    .is_some_and(|notice| notice.key == key && notice.body == body)
                {
                    return false;
                }
            }
            ClientEndpointNoticeKind::Timeout => {
                if !self.endpoint_notice_seen.insert(key.clone()) {
                    return false;
                }
            }
        }
        // A matching notice can return after the previous card was dismissed. Its next draw
        // starts a fresh lifetime instead of inheriting the hidden card's deadline.
        if self
            .visible_endpoint_notice
            .as_ref()
            .is_some_and(|notice| self.restore_notice_seen.contains(&notice.key))
            && let Some(notice) = self.visible_endpoint_notice.take()
        {
            self.restore_notice_queue.push_front(notice);
        }
        self.endpoint_notice_deadline = None;
        self.visible_endpoint_notice = Some(ClientVisibleEndpointNotice {
            key,
            title: title.into(),
            body,
        });
        true
    }

    pub(super) fn push_endpoint_command_with_kind(
        &mut self,
        command: EndpointCommand,
        kind: PendingEndpointKind,
        outcome: &mut ClientShellInput,
    ) -> bool {
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
            return false;
        }
        let method_name = command.name().to_owned();
        let Some(snapshot) = self.snapshot.as_deref() else {
            return false;
        };
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        let request_id = shepr_protocol::RequestId::from(format!("client-shell:{request_id}"));
        self.pending_requests.insert(
            request_id.clone(),
            PendingEndpointRequest {
                boot_id: snapshot.boot_id.clone(),
                method_name,
                kind,
            },
        );
        outcome.actions.push(ClientShellAction::Endpoint {
            endpoint_id: self.active_endpoint_id.clone(),
            boot_id: snapshot.boot_id.clone(),
            request: Box::new(ClientShellEndpointRequest {
                id: request_id.to_string(),
                command,
            }),
        });
        true
    }

    pub(crate) fn receive_paste_rejection(&mut self, message: String) -> bool {
        self.push_endpoint_notice(
            ClientEndpointNoticeKind::Rejected,
            "paste_rejected",
            "Paste rejected",
            message,
        )
    }

    /// Snapshot metadata is accepted from every endpoint, independent of surface ownership.
    /// Keep each boot's card until it has had its own drawn lifetime, even across switches.
    pub(crate) fn receive_restore_notice(
        &mut self,
        endpoint_id: &ClientEndpointId,
        boot_id: &shepr_protocol::BootId,
        notice: &shepr_protocol::SessionRestoreNotice,
    ) -> bool {
        let key = ClientEndpointNoticeKey {
            boot_id: Some(boot_id.clone()),
            kind: ClientEndpointNoticeKind::Rejected,
            code: format!("session_restore_incomplete:{}", endpoint_id.storage_key()),
        };
        if !self.restore_notice_seen.insert(key.clone()) {
            return false;
        }
        let label = self.endpoint_label(endpoint_id);
        self.restore_notice_queue
            .push_back(ClientVisibleEndpointNotice {
                key,
                title: format!("{label}: saved session not fully restored"),
                body: notice.to_string(),
            });
        if self.visible_endpoint_notice.is_none() {
            self.visible_endpoint_notice = self.restore_notice_queue.pop_front();
            self.endpoint_notice_deadline = None;
        }
        true
    }

    /// Shows a notice an endpoint's server sent.
    pub(crate) fn receive_server_notice(&mut self, kind: &shepr_protocol::NoticeKind) -> bool {
        let (code, title) = match kind {
            shepr_protocol::NoticeKind::PaneInputDropped { .. } => (
                "pane_input_dropped".to_owned(),
                "Pane input dropped".to_owned(),
            ),
            shepr_protocol::NoticeKind::PasteRejected { .. } => {
                ("paste_rejected".to_owned(), "Paste rejected".to_owned())
            }
            shepr_protocol::NoticeKind::OversizedSurface { .. } => (
                "oversized_surface".to_owned(),
                "Screen too large".to_owned(),
            ),
        };
        self.push_endpoint_notice(
            ClientEndpointNoticeKind::Rejected,
            code,
            title,
            kind.to_string(),
        )
    }

    pub(crate) fn receive_endpoint_unavailable(&mut self, message: String) -> bool {
        self.push_endpoint_notice_at_boot(
            None,
            ClientEndpointNoticeKind::Unavailable,
            message.clone(),
            "Endpoint unavailable",
            message,
        )
    }

    pub(crate) fn focus_endpoint_target(
        &mut self,
        target: ClientEndpointFocusTarget,
    ) -> Vec<ClientShellAction> {
        let workspace_id = match &target {
            ClientEndpointFocusTarget::Workspace(workspace_id) => Some(workspace_id.clone()),
            ClientEndpointFocusTarget::Pane(_) => None,
        };
        let command = match target {
            ClientEndpointFocusTarget::Workspace(workspace_id) => {
                EndpointCommand::WorkspaceFocus(shepr_protocol::command::WorkspaceTarget {
                    workspace_id,
                })
            }
            ClientEndpointFocusTarget::Pane(pane_id) => {
                EndpointCommand::PaneFocus(shepr_protocol::command::PaneTarget { pane_id })
            }
        };
        let mut outcome = ClientShellInput::default();
        self.push_endpoint_command(command, &mut outcome);
        if let (Some(workspace_id), Some(ClientShellAction::Endpoint { request, .. })) =
            (workspace_id, outcome.actions.first())
            && let Some(target) = self.navigation_target(&self.active_endpoint_id, &workspace_id)
        {
            self.keep_workspace_highlight_until_snapshot(target, &request.id, self.now);
        }
        outcome.actions
    }

    fn cancel_endpoint_request_with_notice(
        &mut self,
        request_id: &str,
        show_cancelled_notice: bool,
    ) -> bool {
        let Some(pending) = self.pending_requests.get(request_id) else {
            return false;
        };
        let boot_id = pending.boot_id.clone();
        // A cancellation is always an error result, and no error path schedules
        // a deadline, so the instant is never compared; it only satisfies the
        // shared result path.
        let outcome = self.handle_endpoint_result_at_with_cancel_notice(
            &boot_id,
            request_id,
            Err(ClientShellEndpointError::Cancelled),
            self.now,
            show_cancelled_notice,
        );
        // Cancellation uses the request kind's rollback, which produces only a repaint.
        // Unlike an ordinary failed reply it must not release buffered input or start work.
        if !(outcome.actions.is_empty() && outcome.requests.is_empty()) {
            tracing::error!("a cancelled endpoint request produced actions or requests");
        }
        outcome.repaint
    }

    pub(crate) fn cancel_endpoint_request(&mut self, request_id: &str) -> bool {
        self.cancel_endpoint_request_with_notice(request_id, true)
    }

    /// Completes a request rejected by the client before it entered the send queue. Its result
    /// is known, so an interruption warning about an unknown server outcome would be misleading.
    pub(crate) fn cancel_unsent_endpoint_request(&mut self, request_id: &str) -> bool {
        self.cancel_endpoint_request_with_notice(request_id, false)
    }

    pub(crate) fn handle_endpoint_result_at(
        &mut self,
        boot_id: &str,
        request_id: &str,
        result: Result<EndpointReply, ClientShellEndpointError>,
        now: std::time::Instant,
    ) -> ClientShellInput {
        self.handle_endpoint_result_at_with_cancel_notice(boot_id, request_id, result, now, true)
    }

    fn handle_endpoint_result_at_with_cancel_notice(
        &mut self,
        boot_id: &str,
        request_id: &str,
        result: Result<EndpointReply, ClientShellEndpointError>,
        now: std::time::Instant,
        show_cancelled_notice: bool,
    ) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        let (repaint, actions) = self.apply_endpoint_result(
            boot_id,
            request_id,
            result,
            &mut outcome,
            now,
            show_cancelled_notice,
        );
        outcome.repaint |= repaint;
        outcome.actions.extend(actions);
        outcome
    }

    fn apply_endpoint_result(
        &mut self,
        boot_id: &str,
        request_id: &str,
        result: Result<EndpointReply, ClientShellEndpointError>,
        outcome: &mut ClientShellInput,
        now: std::time::Instant,
        show_cancelled_notice: bool,
    ) -> (bool, Vec<ClientShellAction>) {
        let Some(pending) = self.pending_requests.remove(request_id) else {
            return (false, Vec::new());
        };
        let cancelled = matches!(&result, Err(ClientShellEndpointError::Cancelled));
        if pending.boot_id != boot_id
            || (!cancelled
                && self
                    .snapshot
                    .as_deref()
                    .is_none_or(|snapshot| snapshot.boot_id != boot_id))
        {
            let highlight_cleared = self
                .pending_workspace_highlight
                .as_ref()
                .is_some_and(|pending| pending.request_id == request_id);
            if highlight_cleared {
                self.pending_workspace_highlight = None;
            }
            return (pending.kind.cancel(self) || highlight_cleared, Vec::new());
        }
        if result.is_ok() {
            let timeout_key = ClientEndpointNoticeKey {
                boot_id: Some(pending.boot_id.clone()),
                kind: ClientEndpointNoticeKind::Timeout,
                code: pending.method_name.clone(),
            };
            self.endpoint_notice_seen.remove(&timeout_key);
        }
        if let Err(error) = &result {
            if self
                .pending_workspace_highlight
                .as_ref()
                .is_some_and(|pending| pending.request_id == request_id)
            {
                self.pending_workspace_highlight = None;
            }
            if (show_cancelled_notice && pending.kind.should_show_cancelled_notice())
                || !matches!(error, ClientShellEndpointError::Cancelled)
            {
                let message = error.to_string();
                let (kind, notice_code, title, body) = match error {
                    ClientShellEndpointError::Timeout => (
                        ClientEndpointNoticeKind::Timeout,
                        pending.method_name.clone(),
                        "Server timed out",
                        format!("This server did not respond to {}.", pending.method_name),
                    ),
                    ClientShellEndpointError::Cancelled => (
                        ClientEndpointNoticeKind::Unavailable,
                        "cancelled".to_owned(),
                        "Action interrupted",
                        message,
                    ),
                    ClientShellEndpointError::Server(EndpointError::ShuttingDown) => (
                        ClientEndpointNoticeKind::Unavailable,
                        "server".to_owned(),
                        "Server unavailable",
                        message,
                    ),
                    ClientShellEndpointError::Server(_) => (
                        ClientEndpointNoticeKind::Rejected,
                        format!("{}:{message}", pending.method_name),
                        "Action rejected",
                        message,
                    ),
                };
                self.push_endpoint_notice_at_boot(
                    Some(pending.boot_id.clone()),
                    kind,
                    notice_code,
                    title,
                    body,
                );
            }
        }
        if cancelled {
            // The ledger entry owns rollback even when its snapshot is no longer presented.
            // Generations and serials in the kind protect newer work from an old cancellation.
            return (pending.kind.cancel(self), Vec::new());
        }
        match pending.kind {
            PendingEndpointKind::Generic => {}
            PendingEndpointKind::WorkspaceLabel { lookup_id } => {
                return (
                    self.complete_workspace_label_lookup(lookup_id, result),
                    Vec::new(),
                );
            }
            PendingEndpointKind::PaneScroll { pane_id, serial } => {
                let repaint = self.complete_pane_scroll(&pane_id, serial, result, now, outcome);
                return (repaint, Vec::new());
            }
            PendingEndpointKind::SelectionCopy => {
                return match result {
                    Ok(EndpointReply::PaneSelection { text, .. }) if !text.is_empty() => (
                        false,
                        vec![ClientShellAction::ClipboardWrite(text.into_bytes())],
                    ),
                    Ok(EndpointReply::PaneSelection { .. }) => {
                        let shown = self.push_endpoint_notice(
                            ClientEndpointNoticeKind::Rejected,
                            "selection_empty",
                            "Nothing copied",
                            "The selection contained no text.",
                        );
                        (shown, Vec::new())
                    }
                    Ok(_) => {
                        self.set_endpoint_error(
                            "endpoint returned an unexpected selection result",
                            now,
                        );
                        (true, Vec::new())
                    }
                    Err(_) => (true, Vec::new()),
                };
            }
            PendingEndpointKind::WordSelection {
                pane_id,
                absolute_row,
                generation,
            } => {
                return self.complete_word_selection_row(
                    &pane_id,
                    absolute_row,
                    generation,
                    result,
                    now,
                );
            }
            PendingEndpointKind::CopyMotion {
                pane_id,
                origin,
                session_generation,
            } => {
                if self.copy_session_generation != session_generation {
                    // The copy session that asked was left, re-entered or abandoned
                    // behind a full input queue; its answer no longer applies.
                    return (false, Vec::new());
                }
                let (repaint, continue_queue) = match result {
                    Ok(EndpointReply::PaneCopyMotion {
                        pane_id: returned_pane_id,
                        cursor,
                    }) if returned_pane_id == pane_id => (
                        self.apply_copy_motion_target(&pane_id, origin, cursor, outcome),
                        true,
                    ),
                    Ok(EndpointReply::PaneCopyMotion { .. }) => (false, false),
                    Ok(_) => {
                        self.set_endpoint_error(
                            "endpoint returned an unexpected copy-motion result",
                            now,
                        );
                        (true, false)
                    }
                    Err(_) => (true, false),
                };
                self.complete_copy_operation(session_generation, continue_queue, outcome);
                return (repaint, Vec::new());
            }
            PendingEndpointKind::CopySearch {
                pane_id,
                origin,
                query,
                direction,
                repeat,
                generation,
                session_generation,
            } => {
                if self.copy_session_generation != session_generation {
                    // As for a copy motion: an older copy session's search result.
                    return (false, Vec::new());
                }
                let (repaint, continue_queue) = match result {
                    Ok(EndpointReply::PaneCopySearch {
                        pane_id: returned_pane_id,
                        matches,
                        total,
                        current,
                        current_global,
                    }) if returned_pane_id == pane_id => {
                        let repaint = self.apply_copy_search_result(
                            &pane_id,
                            origin,
                            query,
                            direction,
                            repeat,
                            generation,
                            ClientCopySearchResult {
                                matches,
                                total,
                                current: current.and_then(|index| usize::try_from(index).ok()),
                                current_global,
                            },
                            outcome,
                        );
                        if !repaint {
                            self.cancel_deferred_copy_after_search(generation);
                        }
                        (repaint, repaint)
                    }
                    Ok(EndpointReply::PaneCopySearch { .. }) => {
                        self.cancel_deferred_copy_after_search(generation);
                        (false, false)
                    }
                    Ok(_) => {
                        self.cancel_deferred_copy_after_search(generation);
                        self.set_endpoint_error(
                            "endpoint returned an unexpected copy-search result",
                            now,
                        );
                        (true, false)
                    }
                    Err(_) => {
                        self.cancel_deferred_copy_after_search(generation);
                        (true, false)
                    }
                };
                self.complete_copy_operation(session_generation, continue_queue, outcome);
                return (repaint, Vec::new());
            }
        }
        // Close confirmation is client-owned (`open_confirm_close_overlay` runs before the
        // close is sent); endpoints close without asking back.
        (result.is_err(), Vec::new())
    }

    pub(super) fn endpoint_command_for_action(
        &mut self,
        action: shepr_termio::input::KeybindAction,
    ) -> Option<EndpointCommand> {
        use shepr_protocol::command::{
            PaneDirection, PaneFocusDirectionParams, PaneResizeParams, PaneSplitParams,
            PaneSwapParams, PaneTarget, PaneZoomParams, SplitDirection, WorkspaceTarget,
        };
        use shepr_termio::input::KeybindAction;

        let snapshot = self.snapshot.as_deref()?;
        let focused_workspace = snapshot.focused_workspace_id.clone()?;
        let focused_pane = snapshot.focused_pane_id.clone();
        let direction = |action| match action {
            KeybindAction::FocusPaneLeft
            | KeybindAction::SwapPaneLeft
            | KeybindAction::ResizePaneLeft => Some(PaneDirection::Left),
            KeybindAction::FocusPaneDown
            | KeybindAction::SwapPaneDown
            | KeybindAction::ResizePaneDown => Some(PaneDirection::Down),
            KeybindAction::FocusPaneUp
            | KeybindAction::SwapPaneUp
            | KeybindAction::ResizePaneUp => Some(PaneDirection::Up),
            KeybindAction::FocusPaneRight
            | KeybindAction::SwapPaneRight
            | KeybindAction::ResizePaneRight => Some(PaneDirection::Right),
            _ => None,
        };

        match action {
            KeybindAction::FocusAgent(_)
            | KeybindAction::PreviousAgent
            | KeybindAction::NextAgent => {
                let agents = super::aggregate_navigation::online_agent_targets(
                    &self.endpoints,
                    &self.active_endpoint_id,
                    self.config.agent_panel_sort,
                );
                let index = super::aggregate_navigation::agent_target_index(
                    &agents,
                    &self.active_endpoint_id,
                    snapshot.focused_pane_id.as_deref(),
                    action,
                )?;
                let target = agents.get(index)?;
                if target.endpoint_id != self.active_endpoint_id {
                    return None;
                }
                let pane_id = target.pane_id.clone();
                // Relative moves can land on a row scrolled out of the sidebar;
                // bring it into view, as a numbered pick already names a shown one.
                if matches!(
                    action,
                    KeybindAction::PreviousAgent | KeybindAction::NextAgent
                ) && !self
                    .hits
                    .agents
                    .iter()
                    .any(|(_, visible_pane_id)| *visible_pane_id == pane_id)
                {
                    self.agent_scroll = index.min(self.hits.agent_max_scroll);
                }
                Some(EndpointCommand::PaneFocus(PaneTarget { pane_id }))
            }
            KeybindAction::SwitchWorkspace(index) => {
                let entries = self.navigation_workspace_entries(snapshot);
                let workspace_id = snapshot
                    .workspaces
                    .get(*entries.get(index)?)?
                    .workspace_id
                    .clone();
                self.reveal_workspace(&workspace_id);
                Some(EndpointCommand::WorkspaceFocus(WorkspaceTarget {
                    workspace_id,
                }))
            }
            KeybindAction::PreviousWorkspace | KeybindAction::NextWorkspace => {
                let entries = self.navigation_workspace_entries(snapshot);
                if entries.is_empty() {
                    return None;
                }
                let current = entries
                    .iter()
                    .position(|entry| snapshot.workspaces[*entry].workspace_id == focused_workspace)
                    .unwrap_or(0);
                let delta = if action == KeybindAction::PreviousWorkspace {
                    -1
                } else {
                    1
                };
                let current_isize = isize::try_from(current).unwrap_or(isize::MAX);
                let len_isize = isize::try_from(entries.len()).unwrap_or(isize::MAX);
                let next = (current_isize + delta).rem_euclid(len_isize) as usize;
                let workspace_id = snapshot.workspaces[entries[next]].workspace_id.clone();
                self.reveal_workspace(&workspace_id);
                Some(EndpointCommand::WorkspaceFocus(WorkspaceTarget {
                    workspace_id,
                }))
            }
            KeybindAction::FocusPaneLeft
            | KeybindAction::FocusPaneDown
            | KeybindAction::FocusPaneUp
            | KeybindAction::FocusPaneRight => Some(EndpointCommand::PaneFocusDirection(
                PaneFocusDirectionParams {
                    pane_id: focused_pane.clone()?,
                    direction: direction(action)?,
                },
            )),
            KeybindAction::SwapPaneLeft
            | KeybindAction::SwapPaneDown
            | KeybindAction::SwapPaneUp
            | KeybindAction::SwapPaneRight => {
                Some(EndpointCommand::PaneSwap(PaneSwapParams::Direction {
                    pane_id: focused_pane.clone()?,
                    direction: direction(action)?,
                }))
            }
            KeybindAction::SplitVertical | KeybindAction::SplitHorizontal => {
                Some(EndpointCommand::PaneSplit(PaneSplitParams {
                    pane_id: focused_pane.clone()?,
                    direction: if action == KeybindAction::SplitVertical {
                        SplitDirection::Right
                    } else {
                        SplitDirection::Down
                    },
                }))
            }
            KeybindAction::ClosePane => Some(EndpointCommand::PaneClose(PaneTarget {
                pane_id: focused_pane.clone()?,
            })),
            KeybindAction::CyclePaneNext | KeybindAction::CyclePanePrevious => {
                let panes = snapshot
                    .panes
                    .iter()
                    .filter(|pane| pane.workspace_id == focused_workspace)
                    .collect::<Vec<_>>();
                if panes.is_empty() {
                    return None;
                }
                let focused_pane = focused_pane?;
                let current = panes
                    .iter()
                    .position(|pane| pane.pane_id == focused_pane)
                    .unwrap_or(0);
                let next = if action == KeybindAction::CyclePanePrevious {
                    (current + panes.len() - 1) % panes.len()
                } else {
                    (current + 1) % panes.len()
                };
                Some(EndpointCommand::PaneFocus(PaneTarget {
                    pane_id: panes[next].pane_id.clone(),
                }))
            }
            KeybindAction::LastPane => {
                let pane_id = self.previous_pane_id.as_ref()?;
                if Some(pane_id.as_str()) == focused_pane.as_deref()
                    || !snapshot.panes.iter().any(|pane| &pane.pane_id == pane_id)
                {
                    return None;
                }
                Some(EndpointCommand::PaneFocus(PaneTarget {
                    pane_id: pane_id.clone(),
                }))
            }
            KeybindAction::Zoom => Some(EndpointCommand::PaneZoom(PaneZoomParams {
                pane_id: focused_pane?,
            })),
            KeybindAction::ClearPane => Some(EndpointCommand::PaneClear(PaneTarget {
                pane_id: focused_pane?,
            })),
            KeybindAction::ResizePaneLeft
            | KeybindAction::ResizePaneDown
            | KeybindAction::ResizePaneUp
            | KeybindAction::ResizePaneRight => {
                Some(EndpointCommand::PaneResize(PaneResizeParams {
                    pane_id: focused_pane?,
                    direction: direction(action)?,
                }))
            }
            _ => None,
        }
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
        self.handle_endpoint_result_at(boot_id, request_id, result, std::time::Instant::now())
    }
}
