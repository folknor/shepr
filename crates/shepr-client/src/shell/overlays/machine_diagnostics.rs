//! Machine status diagnostics. The badge handler sees every raw input event
//! first; input content must stay out of logs and error messages here.

use super::*;
use crossterm::event::{MouseButton, MouseEventKind};
use shepr_termio::input::raw_input::RawInputEvent;

/// Code prefix of a notice opened from a machine badge. Only these explicitly
/// opened cards grow to their full body; automatic notices are capped.
pub(super) const MACHINE_DIAGNOSTIC_NOTICE_PREFIX: &str = "machine-diagnostic:";

#[derive(Default)]
pub(super) struct MachineDiagnostics {
    errors: HashMap<ClientEndpointId, MachineDiagnostic>,
    hover: Option<ClientEndpointId>,
}

/// A machine's last failure: sanitized display text plus the structured SSH
/// class it was reported with, so the text is never re-parsed.
struct MachineDiagnostic {
    message: String,
    requires_authentication: bool,
}

impl MachineDiagnostics {
    pub(super) fn required_for(&self, endpoint: &ClientShellEndpoint) -> bool {
        self.errors
            .get(&endpoint.endpoint_id)
            .is_some_and(|diagnostic| diagnostic.requires_authentication)
    }

    pub(super) fn badge_style(
        &self,
        endpoint: &ClientShellEndpoint,
        palette: &Palette,
        style: Style,
    ) -> Style {
        if self.hover.as_ref() == Some(&endpoint.endpoint_id)
            && self.errors.contains_key(&endpoint.endpoint_id)
        {
            style
                .bg(palette.active_row_bg)
                .add_modifier(Modifier::REVERSED)
        } else {
            style
        }
    }
}

impl ClientShellState {
    pub(crate) fn set_machine_diagnostic(
        &mut self,
        id: &ClientEndpointId,
        failure: &shepr_remote::SshFailureDiagnostic,
    ) {
        self.insert_machine_diagnostic(id, failure.chars(), failure.requires_authentication());
    }

    fn insert_machine_diagnostic(
        &mut self,
        id: &ClientEndpointId,
        message: impl Iterator<Item = char>,
        requires_authentication: bool,
    ) {
        if !id.is_local() {
            self.machine_diagnostics.errors.insert(
                id.clone(),
                MachineDiagnostic {
                    // Preserve line breaks so the notice card can display structured SSH
                    // diagnostics as separate wrapped lines.
                    message: message
                        .filter(|c| !c.is_control() || *c == '\n')
                        .take(crate::limits::MAX_MACHINE_DIAGNOSTIC_CHARS)
                        .collect(),
                    requires_authentication,
                },
            );
        }
    }

    pub(super) fn clear_machine_diagnostic(&mut self, id: &ClientEndpointId) {
        self.machine_diagnostics.errors.remove(id);
    }

    pub(super) fn handle_machine_badge_event(
        &mut self,
        event: &RawInputEvent,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let RawInputEvent::Mouse(mouse) = event else {
            return false;
        };
        if self.overlay.is_some()
            || contains(self.hits.notification_toast, (mouse.column, mouse.row))
        {
            return false;
        }
        let hit = self
            .hits
            .machines
            .iter()
            .find(|hit| {
                contains(hit.status_badge, (mouse.column, mouse.row))
                    && self
                        .machine_diagnostics
                        .errors
                        .contains_key(&hit.endpoint_id)
            })
            .map(|hit| hit.endpoint_id.clone());
        if mouse.kind == MouseEventKind::Moved {
            if self.machine_diagnostics.hover != hit {
                self.machine_diagnostics.hover = hit;
                outcome.repaint = true;
            }
            return false;
        }
        let Some(id) = hit else {
            return false;
        };
        if mouse.kind == MouseEventKind::Up(MouseButton::Left) {
            return true;
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return false;
        }
        let Some(diagnostic) = self.machine_diagnostics.errors.get(&id) else {
            return true;
        };
        let ClientEndpointId::Ssh(label) = &id else {
            return true;
        };
        let code = format!("{MACHINE_DIAGNOSTIC_NOTICE_PREFIX}{label}");
        // An explicit click can reopen its diagnostic, but must not replace another notice.
        if self
            .visible_endpoint_notice
            .as_ref()
            .is_some_and(|notice| notice.key.code != code)
        {
            return true;
        }
        self.endpoint_notice_deadline = None;
        self.visible_endpoint_notice = Some(ClientVisibleEndpointNotice {
            key: ClientEndpointNoticeKey {
                boot_id: None,
                kind: ClientEndpointNoticeKind::Unavailable,
                code,
            },
            // The TUI never prompts: machines connect in BatchMode and
            // interactive authentication runs only at startup, before the TUI
            // takes the terminal. Restarting the client is the way to
            // authenticate on purpose: the servers keep running headless, so
            // every pane and agent is still there when it reattaches, and an
            // in-TUI prompt would need the terminal suspended and resumed
            // around a foreground ssh for no gain.
            title: if diagnostic.requires_authentication {
                format!(
                    "{}: restart shepr to authenticate",
                    self.endpoint_label(&id)
                )
            } else {
                self.endpoint_label(&id).to_owned()
            },
            body: diagnostic.message.clone(),
        });
        outcome.repaint = true;
        true
    }
}
