use super::*;
use crate::protocol::ClientPaneInputEvent;
use crate::raw_input::RawInputEvent;
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

const LOCAL_INPUT_SOURCE: u8 = 0;

fn is_retained_selection_copy_key(key: &crate::input::TerminalKey) -> bool {
    matches!(key.code, KeyCode::Char('c' | 'C'))
        && matches!(key.modifiers, KeyModifiers::CONTROL | KeyModifiers::SUPER)
}

/// How long Ctrl+V in a modal input waits for the clipboard helper before giving up.
const MODAL_PASTE_CLIPBOARD_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Set while a clipboard helper thread is still running, including one abandoned after a
/// timeout, so a hung helper is waited on once rather than once per keypress.
static CLIPBOARD_READ_IN_FLIGHT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Reads the host clipboard for a modal paste without letting a hung helper stall the client.
///
/// Key routing runs on the client's event loop, so a clipboard owner that never answers
/// (e.g. `xclip -out` against an unresponsive X selection owner) would freeze rendering and
/// input for every pane. The platform reader has no timeout of its own and cannot be
/// cancelled, so it runs on its own thread and the paste is abandoned after
/// `MODAL_PASTE_CLIPBOARD_TIMEOUT`. An abandoned read keeps its thread (and helper process)
/// until the helper exits; later pastes are skipped immediately until then.
fn read_clipboard_text_bounded() -> Option<String> {
    read_clipboard_text_bounded_with(
        &CLIPBOARD_READ_IN_FLIGHT,
        MODAL_PASTE_CLIPBOARD_TIMEOUT,
        crate::platform::read_clipboard_text,
    )
}

fn read_clipboard_text_bounded_with(
    in_flight: &'static std::sync::atomic::AtomicBool,
    timeout: std::time::Duration,
    read: fn() -> Option<String>,
) -> Option<String> {
    use std::sync::atomic::Ordering;

    if in_flight.swap(true, Ordering::AcqRel) {
        tracing::warn!("an earlier clipboard read is still running; paste skipped");
        return None;
    }
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("shepr-clipboard-read".into())
        .spawn(move || {
            let text = read();
            in_flight.store(false, Ordering::Release);
            let _ = sender.send(text);
        });
    if let Err(error) = spawned {
        in_flight.store(false, Ordering::Release);
        tracing::warn!(%error, "could not start the clipboard reader; paste skipped");
        return None;
    }
    match receiver.recv_timeout(timeout) {
        Ok(text) => text,
        Err(_) => {
            tracing::warn!(
                timeout_ms = timeout.as_millis(),
                "clipboard helper did not answer in time; paste skipped"
            );
            None
        }
    }
}

pub(super) fn is_modal_paste_shortcut(key: &crate::input::TerminalKey) -> bool {
    key.generated_text.as_deref().is_none_or(str::is_empty)
        && matches!(key.code, KeyCode::Char('v' | 'V'))
        && key.modifiers.difference(KeyModifiers::SHIFT) == KeyModifiers::CONTROL
}

fn host_theme_update(event: &RawInputEvent) -> Option<crate::protocol::ClientHostThemeUpdate> {
    use crate::protocol::{
        ClientHostAppearance, ClientHostDefaultColorKind, ClientHostThemeUpdate,
    };

    match event {
        RawInputEvent::HostDefaultColor { kind, color } => {
            Some(ClientHostThemeUpdate::DefaultColor {
                kind: match kind {
                    crate::terminal_theme::DefaultColorKind::Foreground => {
                        ClientHostDefaultColorKind::Foreground
                    }
                    crate::terminal_theme::DefaultColorKind::Background => {
                        ClientHostDefaultColorKind::Background
                    }
                },
                color: (*color).into(),
            })
        }
        RawInputEvent::HostPaletteColors { colors } => Some(ClientHostThemeUpdate::PaletteColors(
            colors
                .iter()
                .map(|(index, color)| (*index, (*color).into()))
                .collect(),
        )),
        RawInputEvent::HostColorSchemeChanged(appearance) => {
            Some(ClientHostThemeUpdate::Appearance(match appearance {
                crate::terminal_theme::HostAppearance::Dark => ClientHostAppearance::Dark,
                crate::terminal_theme::HostAppearance::Light => ClientHostAppearance::Light,
            }))
        }
        _ => None,
    }
}

fn push_host_theme_update(
    requests: &mut Vec<ClientMessage>,
    update: crate::protocol::ClientHostThemeUpdate,
) {
    if let crate::protocol::ClientHostThemeUpdate::PaletteColors(colors) = &update
        && let Some(ClientMessage::ClientShellHostTheme {
            update: crate::protocol::ClientHostThemeUpdate::PaletteColors(pending),
        }) = requests.last_mut()
        && pending.len() + colors.len() <= 256
    {
        pending.extend_from_slice(colors);
        return;
    }
    requests.push(ClientMessage::ClientShellHostTheme { update });
}

impl ClientShellState {
    pub(crate) fn host_keyboard_report_all_requested(&self) -> bool {
        matches!(
            self.mode,
            ClientShellMode::Prefix | ClientShellMode::Navigate
        )
    }

    #[cfg(test)]
    pub(crate) fn handle_input_bytes(&mut self, data: &[u8]) -> ClientShellInput {
        self.handle_raw_events(crate::raw_input::parse_raw_input_bytes_sync(data))
    }

    pub(crate) fn handle_pixel_mouse(
        &mut self,
        data: &[u8],
        geometry: crate::input::mouse::HostGeometry,
    ) -> ClientShellInput {
        let Some((x, y)) = crate::input::mouse::parse_report(data) else {
            return ClientShellInput::default();
        };
        let Some((column, row)) = geometry.cell(x, y) else {
            return ClientShellInput::default();
        };
        let Some(cell_report) = crate::input::mouse::report_at_cell(data, column, row) else {
            return ClientShellInput::default();
        };
        let events = crate::raw_input::parse_raw_input_bytes_sync(&cell_report);
        if events.len() != 1 || !matches!(events[0], RawInputEvent::Mouse(_)) {
            return ClientShellInput::default();
        }
        self.host_mouse_pixels = Some(crate::input::mouse::HostPixels { x, y, geometry });
        let outcome = self.handle_raw_events(events);
        self.host_mouse_pixels = None;
        outcome
    }

    fn prepare_committed_text(&mut self, text: &str, outcome: &mut ClientShellInput) -> bool {
        if !(self.mode == ClientShellMode::Navigate && self.workspace_preview_action_blocked())
            && self.insert_copy_search_text(text)
        {
            outcome.repaint = true;
            return true;
        }
        self.word_selection_gesture = None;
        if self.copy_or_terminal_mode() != ClientShellMode::Copy && self.selection.take().is_some()
        {
            self.stop_selection_autoscroll();
            self.selection_highlight_clear_deadline = None;
            outcome.repaint = true;
        }
        false
    }

    pub(crate) fn handle_raw_events(&mut self, events: Vec<RawInputEvent>) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        if !events.is_empty() && self.endpoint_error.take().is_some() {
            self.endpoint_error_deadline = None;
            outcome.repaint = true;
        }
        for event in events {
            if self.handle_machine_badge_event(&event, &mut outcome) {
                continue;
            }
            if let Some(update) = host_theme_update(&event) {
                push_host_theme_update(&mut outcome.requests, update);
            }
            match event {
                RawInputEvent::Key(key) => self.handle_key(key, &mut outcome),
                RawInputEvent::Text(text) => {
                    let text = text.into_string();
                    if self.prepare_committed_text(&text, &mut outcome) {
                        continue;
                    }
                    if self.insert_overlay_text(&text) {
                        outcome.repaint = true;
                    } else if self.overlay.is_none() && self.mode == ClientShellMode::Terminal {
                        self.push_focused_pane_event(
                            ClientPaneInputEvent::TextCommit(text),
                            &mut outcome,
                        );
                    }
                }
                RawInputEvent::Paste(text) => {
                    if self.prepare_committed_text(&text, &mut outcome) {
                        continue;
                    }
                    if self.insert_overlay_text(&text) {
                        outcome.repaint = true;
                    } else if self.overlay.is_none() && self.mode == ClientShellMode::Terminal {
                        self.push_focused_paste(text, &mut outcome);
                    }
                }
                RawInputEvent::Mouse(mouse) => self.handle_mouse(mouse, &mut outcome),
                RawInputEvent::OuterFocusGained => {
                    self.outer_focused = Some(true);
                    outcome.query_host_appearance = true;
                    if self.config.redraw_on_focus_gained {
                        outcome.repaint = true;
                        outcome.full_redraw = true;
                    }
                    if let Some(surface) = self.pane_surface.clone() {
                        outcome.repaint |= self.acknowledge_active_surface_agents(&surface);
                    }
                    outcome
                        .requests
                        .push(ClientMessage::ClientShellFocus { focused: true });
                }
                RawInputEvent::OuterFocusLost => {
                    self.outer_focused = Some(false);
                    self.release_input_leases(&mut outcome);
                    outcome
                        .requests
                        .push(ClientMessage::ClientShellFocus { focused: false });
                }
                RawInputEvent::HostDefaultColor {
                    kind: crate::terminal_theme::DefaultColorKind::Background,
                    color,
                } => {
                    if self.host_background != Some(color) {
                        self.host_background = Some(color);
                        outcome.repaint = true;
                    }
                }
                RawInputEvent::HostColorSchemeChanged(_) => {
                    // A dark/light switch changes the host's default and palette
                    // colours too. Re-query them so panes and the selection
                    // highlight (`host_background`) follow. Direct attach does the
                    // same; the stdin framer arms itself for the replies whenever
                    // it tracks scheme changes.
                    outcome.query_host_theme = true;
                }
                RawInputEvent::HostDefaultColor { .. }
                | RawInputEvent::HostPaletteColors { .. }
                | RawInputEvent::HostCellSizeReport { .. }
                | RawInputEvent::Unsupported => {}
            }
        }
        outcome
    }

    pub(super) fn handle_key(
        &mut self,
        key: crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        if self.copy_operation_in_flight {
            self.copy_input_queue.push_back(key);
            return;
        }
        let lease_key = crate::input::InputLeaseKey::new(LOCAL_INPUT_SOURCE, &key);
        let key = self.input_leases.normalize_press(&lease_key, key);
        match key.kind {
            KeyEventKind::Press => {
                let initial_context = self.input_context();
                let target = self.route_key_press(&key, outcome);
                if let Some(target) = target.as_ref() {
                    self.push_pane_key(target.clone(), key.clone(), outcome);
                }
                let resulting_context = self.input_context();
                let plan = self.input_leases.complete_press(
                    lease_key,
                    &key,
                    Some(&initial_context),
                    Some(&resulting_context),
                    target,
                );
                self.execute_repeat_plan(lease_key, key, plan, outcome);
            }
            KeyEventKind::Repeat => {
                let context = self.input_context();
                let plan = self
                    .input_leases
                    .plan_repeat(lease_key, &key, Some(&context));
                self.execute_repeat_plan(lease_key, key, plan, outcome);
            }
            KeyEventKind::Release => {
                if let Some(lease) = self.input_leases.remove_forwarded(&lease_key) {
                    let release = lease
                        .key
                        .with_modifiers(key.modifiers)
                        .with_kind(KeyEventKind::Release);
                    self.push_pane_key(lease.target, release, outcome);
                } else {
                    let _ = self.input_leases.remove(&lease_key);
                }
            }
        }
    }

    fn release_input_leases(&mut self, outcome: &mut ClientShellInput) {
        for lease in self.input_leases.remove_source(LOCAL_INPUT_SOURCE) {
            self.push_pane_key(
                lease.target,
                lease.key.with_kind(KeyEventKind::Release),
                outcome,
            );
        }
        if let Some(gesture) = self.pane_mouse_gesture.take() {
            let modifiers = gesture
                .last_event
                .modifiers
                .difference(gesture.stripped_modifiers);
            let geometry = matches!(
                gesture.last_position,
                crate::protocol::ClientMousePosition::Pixels { .. }
            )
            .then_some(crate::protocol::ClientMouseGeometry {
                cols: gesture.hit.inner_rect.width,
                rows: gesture.hit.inner_rect.height,
                width_px: gesture.hit.pixel_width,
                height_px: gesture.hit.pixel_height,
            });
            super::push_target_event(
                ClientInputTarget::Pane(gesture.hit.pane_id),
                ClientPaneInputEvent::Mouse {
                    kind: crate::protocol::ClientMouseKind::Up(
                        crate::protocol::ClientMouseButton::from_crossterm(gesture.button),
                    ),
                    position: gesture.last_position,
                    geometry,
                    modifiers: modifiers.bits(),
                    lines: u16::try_from(self.config.mouse_scroll_lines).unwrap_or(u16::MAX),
                },
                outcome,
            );
        }
        self.copy_input_queue.clear();
    }

    fn execute_repeat_plan(
        &mut self,
        lease_key: crate::input::InputLeaseKey<u8>,
        key: crate::input::TerminalKey,
        plan: crate::input::RepeatPlan<ClientInputContext, ClientInputTarget>,
        outcome: &mut ClientShellInput,
    ) {
        match plan {
            crate::input::RepeatPlan::Forwarded(target) => {
                self.push_pane_key(target, key, outcome);
            }
            crate::input::RepeatPlan::Reprocess {
                context,
                repetitions,
                tracked,
            } => {
                for _ in 0..repetitions {
                    let current = self.input_context();
                    if !self.input_leases.reprocess_allowed(
                        lease_key,
                        &context,
                        Some(&current),
                        tracked,
                    ) {
                        break;
                    }
                    let repeated = key
                        .clone()
                        .with_repeat_count(1)
                        .with_kind(KeyEventKind::Repeat);
                    if let Some(target) = self.route_key_press(&repeated, outcome) {
                        self.push_pane_key(target, repeated, outcome);
                    }
                }
            }
            crate::input::RepeatPlan::Ignore => {}
        }
    }

    pub(super) fn modal_paste_target_active(&self) -> bool {
        if self.overlay.is_none()
            && self.mode == ClientShellMode::Navigate
            && self.workspace_preview_action_blocked()
        {
            return false;
        }
        if self.mode == ClientShellMode::Copy
            && self.overlay.is_none()
            && self
                .copy_mode
                .as_ref()
                .is_some_and(|copy_mode| copy_mode.search_prompt.is_some())
        {
            return true;
        }
        matches!(
            self.overlay.as_ref(),
            Some(ClientShellOverlay::Rename(_))
                | Some(ClientShellOverlay::Navigator(ClientNavigatorOverlay {
                    search_focused: true,
                    ..
                }))
                | Some(ClientShellOverlay::Help(ClientHelpOverlay {
                    search_focused: true,
                    ..
                }))
        )
    }

    pub(super) fn handle_modal_paste_shortcut_with(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
        read_clipboard_text: impl FnOnce() -> Option<String>,
    ) -> bool {
        if !is_modal_paste_shortcut(key) || !self.modal_paste_target_active() {
            return false;
        }
        if let Some(text) = read_clipboard_text() {
            let inserted = self.insert_copy_search_text(&text) || self.insert_overlay_text(&text);
            outcome.repaint |= inserted;
        }
        true
    }

    fn route_key_press(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> Option<ClientInputTarget> {
        if self.handle_modal_paste_shortcut_with(key, outcome, read_clipboard_text_bounded) {
            return None;
        }
        if self.overlay.is_some() {
            self.route_overlay_key(key, outcome);
            return None;
        }
        if matches!(key.code, KeyCode::Modifier(_)) {
            return None;
        }
        self.word_selection_gesture = None;
        if self.mode != ClientShellMode::Copy
            && self.copy_or_terminal_mode() != ClientShellMode::Copy
            && !self.config.copy_on_select
            && is_retained_selection_copy_key(key)
            && self
                .selection
                .as_ref()
                .is_some_and(crate::selection::Selection::is_visible)
        {
            self.request_selection_copy(outcome, true);
            self.selection = None;
            self.stop_selection_autoscroll();
            self.selection_highlight_clear_deadline = None;
            outcome.repaint = true;
            return None;
        }
        if self.mode != ClientShellMode::Copy
            && self.copy_or_terminal_mode() != ClientShellMode::Copy
            && self.selection.take().is_some()
        {
            self.stop_selection_autoscroll();
            self.selection_highlight_clear_deadline = None;
            outcome.repaint = true;
        }

        match self.mode {
            ClientShellMode::Terminal => {
                if let Some(binding) =
                    crate::input::resolve_direct_binding(&self.config.keybinds.keybinds, key)
                {
                    self.record_binding(&binding, outcome);
                    return None;
                }
                if crate::config::terminal_key_matches_combo(key, self.config.keybinds.prefix) {
                    self.mode = ClientShellMode::Prefix;
                    outcome.repaint = true;
                    return None;
                }
                self.focused_pane_id().map(ClientInputTarget::Pane)
            }
            ClientShellMode::Prefix => {
                let return_mode = if self.copy_mode.as_ref().is_some_and(|copy_mode| {
                    self.focused_pane_id().as_deref() == Some(copy_mode.pane_id.as_str())
                }) {
                    ClientShellMode::Copy
                } else {
                    ClientShellMode::Terminal
                };
                if crate::config::terminal_key_matches_combo(key, self.config.keybinds.prefix) {
                    self.mode = return_mode;
                    outcome.repaint = true;
                    return self.focused_pane_id().map(ClientInputTarget::Pane);
                }
                if key.code == KeyCode::Esc {
                    self.mode = return_mode;
                    outcome.repaint = true;
                    return None;
                }
                if let Some(binding) =
                    crate::input::resolve_prefix_binding(&self.config.keybinds.keybinds, key)
                {
                    self.mode = return_mode;
                    outcome.repaint = true;
                    self.record_binding(&binding, outcome);
                    return None;
                }
                self.mode = return_mode;
                outcome.repaint = true;
                None
            }
            ClientShellMode::Navigate => {
                self.route_navigate_key(key, outcome);
                None
            }
            ClientShellMode::Resize => {
                self.route_resize_key(key, outcome);
                None
            }
            ClientShellMode::Copy => {
                if self
                    .copy_mode
                    .as_ref()
                    .is_none_or(|copy_mode| copy_mode.search_prompt.is_none())
                    && crate::config::terminal_key_matches_combo(key, self.config.keybinds.prefix)
                {
                    self.mode = ClientShellMode::Prefix;
                    outcome.repaint = true;
                } else {
                    self.route_copy_mode_key(key, outcome);
                }
                None
            }
        }
    }

    pub(super) fn copy_or_terminal_mode(&self) -> ClientShellMode {
        if self.copy_mode.as_ref().is_some_and(|copy_mode| {
            self.focused_pane_id().as_deref() == Some(copy_mode.pane_id.as_str())
        }) {
            ClientShellMode::Copy
        } else {
            ClientShellMode::Terminal
        }
    }

    fn route_navigate_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        use crate::input::{KeybindAction, KeybindDispatch, KeybindMatch};

        self.pending_workspace_highlight = None;
        if key.code == KeyCode::Esc
            || crate::config::terminal_key_matches_combo(key, self.config.keybinds.prefix)
        {
            self.mode = self.copy_or_terminal_mode();
            self.navigate_workspace_id = None;
            outcome.repaint = true;
            return;
        }

        if self
            .config
            .keybinds
            .keybinds
            .navigate
            .workspace_up
            .matches_direct_key(key)
        {
            self.move_navigate_workspace(-1);
            outcome.repaint = true;
            return;
        }
        if self
            .config
            .keybinds
            .keybinds
            .navigate
            .workspace_down
            .matches_direct_key(key)
        {
            self.move_navigate_workspace(1);
            outcome.repaint = true;
            return;
        }

        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        if code == KeyCode::Enter && modifiers.is_empty() {
            self.accept_navigate_workspace(outcome);
            return;
        }
        if self.workspace_preview_action_blocked() {
            self.push_endpoint_notice(
                ClientEndpointNoticeKind::Rejected,
                "navigate_endpoint_inactive",
                "Confirm workspace first",
                "Select an available workspace and press Enter before using workspace or pane actions",
            );
            outcome.repaint = true;
            return;
        }

        if let Some(index) = ('1'..='9').position(|digit| {
            crate::config::terminal_key_matches_combo(
                key,
                (KeyCode::Char(digit), KeyModifiers::empty()),
            )
        }) {
            let valid = self.snapshot.as_deref().is_some_and(|snapshot| {
                self.navigation_workspace_entries(snapshot)
                    .get(index)
                    .is_some()
            });
            if valid {
                self.mode = ClientShellMode::Terminal;
                self.navigate_workspace_id = None;
                self.record_binding(
                    &KeybindMatch::Action(KeybindAction::SwitchWorkspace(index)),
                    outcome,
                );
                outcome.repaint = true;
            }
            return;
        }

        if modifiers.is_empty() {
            match code {
                KeyCode::Tab => {
                    self.record_navigate_binding(
                        &KeybindMatch::Action(KeybindAction::CyclePaneNext),
                        false,
                        outcome,
                    );
                    return;
                }
                KeyCode::BackTab => {
                    self.record_navigate_binding(
                        &KeybindMatch::Action(KeybindAction::CyclePanePrevious),
                        false,
                        outcome,
                    );
                    return;
                }
                KeyCode::Left => {
                    self.record_navigate_binding(
                        &KeybindMatch::Action(KeybindAction::FocusPaneLeft),
                        true,
                        outcome,
                    );
                    return;
                }
                KeyCode::Right => {
                    self.record_navigate_binding(
                        &KeybindMatch::Action(KeybindAction::FocusPaneRight),
                        true,
                        outcome,
                    );
                    return;
                }
                _ => {}
            }
        }

        let pane_action = [
            (
                &self.config.keybinds.keybinds.navigate.pane_left,
                KeybindAction::FocusPaneLeft,
            ),
            (
                &self.config.keybinds.keybinds.navigate.pane_down,
                KeybindAction::FocusPaneDown,
            ),
            (
                &self.config.keybinds.keybinds.navigate.pane_up,
                KeybindAction::FocusPaneUp,
            ),
            (
                &self.config.keybinds.keybinds.navigate.pane_right,
                KeybindAction::FocusPaneRight,
            ),
        ]
        .into_iter()
        .find_map(|(bindings, action)| bindings.matches_direct_key(key).then_some(action));
        if let Some(action) = pane_action {
            self.record_navigate_binding(&KeybindMatch::Action(action), true, outcome);
            return;
        }

        let binding = crate::input::resolve_non_indexed_action(
            &self.config.keybinds.keybinds,
            key,
            KeybindDispatch::Prefix,
        )
        .filter(|action| {
            !matches!(
                action,
                KeybindAction::FocusPaneLeft
                    | KeybindAction::FocusPaneDown
                    | KeybindAction::FocusPaneUp
                    | KeybindAction::FocusPaneRight
            )
        })
        .map(KeybindMatch::Action)
        .or_else(|| {
            crate::input::resolve_indexed_action(
                &self.config.keybinds.keybinds,
                key,
                KeybindDispatch::Prefix,
            )
            .map(KeybindMatch::Action)
        });
        if let Some(binding) = binding {
            self.record_navigate_binding(&binding, false, outcome);
        }
    }

    fn record_navigate_binding(
        &mut self,
        binding: &crate::input::KeybindMatch,
        preserve_navigate: bool,
        outcome: &mut ClientShellInput,
    ) {
        use crate::input::{KeybindAction, KeybindMatch};

        if !self.indexed_navigation_target_exists(binding) {
            return;
        }
        if let KeybindMatch::Action(KeybindAction::CyclePaneNext) = binding {
            self.cycle_pane(false, outcome);
        } else if let KeybindMatch::Action(KeybindAction::CyclePanePrevious) = binding {
            self.cycle_pane(true, outcome);
        } else {
            if !preserve_navigate {
                self.mode = ClientShellMode::Terminal;
            }
            self.record_binding(binding, outcome);
            // Navigate mode was left just above, but a close dialog opened from
            // it should still cancel back into it.
            if let Some(ClientShellOverlay::ConfirmClose(confirm)) = self.overlay.as_mut() {
                confirm.return_to_navigate = true;
            }
        }
        if !preserve_navigate {
            if self.mode == ClientShellMode::Navigate {
                self.mode = self.copy_or_terminal_mode();
            }
            self.navigate_workspace_id = None;
        }
        outcome.repaint = true;
    }

    pub(super) fn indexed_navigation_target_exists(
        &self,
        binding: &crate::input::KeybindMatch,
    ) -> bool {
        use crate::input::{KeybindAction, KeybindMatch};

        match binding {
            KeybindMatch::Action(KeybindAction::SwitchWorkspace(index)) => {
                self.snapshot.as_deref().is_some_and(|snapshot| {
                    self.navigation_workspace_entries(snapshot)
                        .get(*index)
                        .is_some()
                })
            }
            KeybindMatch::Action(KeybindAction::SwitchTab(index)) => self
                .snapshot
                .as_deref()
                .and_then(|snapshot| {
                    let workspace_id = snapshot.focused_workspace_id.as_deref()?;
                    snapshot
                        .tabs
                        .iter()
                        .filter(|tab| tab.workspace_id == workspace_id)
                        .nth(*index)
                })
                .is_some(),
            KeybindMatch::Action(KeybindAction::FocusAgent(index)) => {
                super::aggregate_navigation::online_agent_targets(
                    &self.endpoints,
                    &self.active_endpoint_id,
                    self.config.agent_panel_sort,
                )
                .get(*index)
                .is_some()
            }
            _ => true,
        }
    }

    fn cycle_pane(&mut self, reverse: bool, outcome: &mut ClientShellInput) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let Some(surface) = self.pane_surface.as_ref() else {
            return;
        };
        if surface.panes.is_empty() {
            return;
        }
        let current = snapshot
            .focused_pane_id
            .as_deref()
            .and_then(|focused| {
                surface
                    .panes
                    .iter()
                    .position(|pane| pane.pane_id == focused)
            })
            .unwrap_or(0);
        let next = if reverse {
            (current + surface.panes.len() - 1) % surface.panes.len()
        } else {
            (current + 1) % surface.panes.len()
        };
        let pane_id = surface.panes[next].pane_id.clone();
        self.push_endpoint_method(
            crate::api::schema::Method::PaneFocus(crate::api::schema::PaneTarget { pane_id }),
            outcome,
        );
    }

    fn route_resize_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        let resize_bindings = &self.config.keybinds.keybinds.resize_mode;
        if key.code == KeyCode::Esc
            || key.code == KeyCode::Enter
            || resize_bindings.matches_prefix_key(key)
            || resize_bindings.matches_direct_key(key)
        {
            self.mode = self.copy_or_terminal_mode();
            outcome.repaint = true;
            return;
        }

        let action = match key.code {
            KeyCode::Char('h') | KeyCode::Left => Some(crate::input::KeybindAction::ResizePaneLeft),
            KeyCode::Char('j') | KeyCode::Down => Some(crate::input::KeybindAction::ResizePaneDown),
            KeyCode::Char('k') | KeyCode::Up => Some(crate::input::KeybindAction::ResizePaneUp),
            KeyCode::Char('l') | KeyCode::Right => {
                Some(crate::input::KeybindAction::ResizePaneRight)
            }
            _ => None,
        };
        if let Some(action) = action {
            self.record_binding(&crate::input::KeybindMatch::Action(action), outcome);
        }
    }

    fn input_context(&self) -> ClientInputContext {
        ClientInputContext {
            mode: self.mode,
            overlay: self.overlay.as_ref().map(ClientShellOverlay::kind),
            retained_selection: self
                .selection
                .as_ref()
                .is_some_and(crate::selection::Selection::is_visible),
        }
    }

    pub(super) fn focused_pane_id(&self) -> Option<String> {
        self.snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.focused_pane_id.clone())
    }

    fn push_pane_key(
        &self,
        target: ClientInputTarget,
        key: crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        if let Some(event) = ClientPaneInputEvent::from_terminal_key(key) {
            super::push_target_event(target, event, outcome);
        }
    }

    fn push_focused_pane_event(&self, event: ClientPaneInputEvent, outcome: &mut ClientShellInput) {
        if let Some(pane_id) = self.focused_pane_id() {
            super::push_target_event(ClientInputTarget::Pane(pane_id), event, outcome);
        }
    }

    /// Sends a paste to the focused pane, or rejects it locally when the
    /// server would.
    ///
    /// The server caps the text of one input message at `MAX_INPUT_PAYLOAD`.
    /// Checking here means an oversized paste never goes out: a paste past the
    /// frame cap would otherwise make the server drop the connection, and
    /// anything over the limit would only come back as a rejection anyway. A
    /// paste that fits on its own but would push an already batched message
    /// past the limit starts a message of its own instead of joining the batch.
    fn push_focused_paste(&mut self, text: String, outcome: &mut ClientShellInput) {
        let size = text.len();
        if size > crate::protocol::MAX_INPUT_PAYLOAD {
            outcome.repaint |= self.receive_endpoint_error(format!(
                "Paste is {size} bytes; Shepr's limit is {} bytes",
                crate::protocol::MAX_INPUT_PAYLOAD
            ));
            return;
        }
        let Some(pane_id) = self.focused_pane_id() else {
            return;
        };
        let batched = match outcome.requests.last() {
            Some(ClientMessage::ClientShellPaneInput {
                pane_id: pending,
                events,
            }) if *pending == pane_id => events
                .iter()
                .map(ClientPaneInputEvent::text_bytes)
                .fold(0usize, usize::saturating_add),
            _ => 0,
        };
        let event = ClientPaneInputEvent::Paste(text);
        if batched.saturating_add(size) > crate::protocol::MAX_INPUT_PAYLOAD {
            outcome.requests.push(super::target_event_message(
                ClientInputTarget::Pane(pane_id),
                event,
            ));
        } else {
            super::push_target_event(ClientInputTarget::Pane(pane_id), event, outcome);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::MAX_INPUT_PAYLOAD;

    fn shell() -> ClientShellState {
        let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
        state.set_snapshot(Box::new(super::super::tests::snapshot()));
        state
    }

    fn message_text_bytes(message: &ClientMessage) -> usize {
        let ClientMessage::ClientShellPaneInput { events, .. } = message else {
            panic!("expected targeted pane input, got {message:?}");
        };
        events.iter().map(ClientPaneInputEvent::text_bytes).sum()
    }

    #[test]
    fn paste_over_the_input_limit_is_rejected_locally_and_never_sent() {
        let mut state = shell();
        let outcome = state.handle_raw_events(vec![RawInputEvent::Paste(
            "x".repeat(MAX_INPUT_PAYLOAD + 1),
        )]);
        assert!(outcome.requests.is_empty());

        let at_limit =
            state.handle_raw_events(vec![RawInputEvent::Paste("x".repeat(MAX_INPUT_PAYLOAD))]);
        assert_eq!(at_limit.requests.len(), 1);
        assert_eq!(message_text_bytes(&at_limit.requests[0]), MAX_INPUT_PAYLOAD);
    }

    #[test]
    fn pastes_that_would_overflow_one_message_go_out_separately() {
        let mut state = shell();
        let half = MAX_INPUT_PAYLOAD / 2 + 1;
        let outcome = state.handle_raw_events(vec![
            RawInputEvent::Paste("a".repeat(half)),
            RawInputEvent::Paste("b".repeat(half)),
        ]);
        assert_eq!(outcome.requests.len(), 2);
        for request in &outcome.requests {
            assert!(message_text_bytes(request) <= MAX_INPUT_PAYLOAD);
        }

        let small = state.handle_raw_events(vec![
            RawInputEvent::Paste("a".into()),
            RawInputEvent::Paste("b".into()),
        ]);
        assert_eq!(small.requests.len(), 1, "small pastes still batch");
    }

    #[test]
    fn bounded_clipboard_read_returns_a_prompt_answer() {
        static IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let text =
            read_clipboard_text_bounded_with(&IN_FLIGHT, std::time::Duration::from_secs(5), || {
                Some("clip".to_owned())
            });
        assert_eq!(text.as_deref(), Some("clip"));
    }

    #[test]
    fn hung_clipboard_helper_is_abandoned_and_not_waited_on_again() {
        static IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        fn hung() -> Option<String> {
            std::thread::sleep(std::time::Duration::from_millis(400));
            Some("late".to_owned())
        }

        let started = std::time::Instant::now();
        let first = read_clipboard_text_bounded_with(
            &IN_FLIGHT,
            std::time::Duration::from_millis(20),
            hung,
        );
        assert!(first.is_none());
        // The abandoned reader is still running: the next paste gives up at once.
        let second =
            read_clipboard_text_bounded_with(&IN_FLIGHT, std::time::Duration::from_secs(5), hung);
        assert!(second.is_none());
        assert!(started.elapsed() < std::time::Duration::from_millis(300));

        // Once the helper exits, reads work again.
        while IN_FLIGHT.load(std::sync::atomic::Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let third =
            read_clipboard_text_bounded_with(&IN_FLIGHT, std::time::Duration::from_secs(5), || {
                Some("clip".to_owned())
            });
        assert_eq!(third.as_deref(), Some("clip"));
    }
}
