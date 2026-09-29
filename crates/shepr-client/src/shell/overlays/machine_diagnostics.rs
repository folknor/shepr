//! Machine status diagnostics. The badge handler sees every raw input event
//! first; input content must stay out of logs and error messages here.

use super::*;
use crossterm::event::{MouseButton, MouseEventKind};
use shepr_termio::input::raw_input::RawInputEvent;

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

    /// Record a non-SSH failure (such as an undecodable endpoint config) as
    /// the machine's diagnostic.
    pub(super) fn set_machine_error(&mut self, id: &ClientEndpointId, message: &str) {
        self.insert_machine_diagnostic(id, message.chars(), false);
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
        let ClientEndpointId::Ssh(profile_id) = &id else {
            return true;
        };
        let command = if diagnostic.requires_authentication {
            format!("shepr machine reconnect {profile_id}")
        } else {
            format!("shepr machine status {profile_id}")
        };
        let code = format!("machine-diagnostic:{profile_id}");
        // An explicit click can reopen its diagnostic, but must not replace another notice.
        if self
            .visible_endpoint_notice
            .as_ref()
            .is_some_and(|notice| notice.key.code != code)
        {
            return true;
        }
        self.visible_endpoint_notice = Some(ClientVisibleEndpointNotice {
            key: ClientEndpointNoticeKey {
                boot_id: "machine".into(),
                kind: ClientEndpointNoticeKind::Unavailable,
                code,
            },
            title: format!("{}: {command}", self.endpoint_label(&id)),
            body: diagnostic.message.clone(),
        });
        outcome.repaint = true;
        true
    }
}
