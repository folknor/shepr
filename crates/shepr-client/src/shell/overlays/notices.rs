//! Notice suppression, boot-card queueing and drawn lifetimes have one owner.

use crate::endpoint::ClientEndpointId;
use shepr_protocol::command::CommandKind;
use std::collections::{HashSet, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::shell) enum ClientEndpointNoticeKind {
    Rejected,
    Timeout,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::shell) enum NoticeCode {
    SelectionEmpty,
    PasteRejected,
    NavigateEndpointInactive,
    Server,
    Cancelled,
    Command(CommandKind),
    SessionRestoreIncomplete,
    SessionSavesStopped,
    PaneInputDropped,
    OversizedSurface,
    SizeLimit,
    MachineDiagnostic,
    EndpointUnavailable,
}

#[derive(Clone, Copy)]
enum Deduplication {
    None,
    VisibleBody,
    UntilSuccess,
}

impl NoticeCode {
    fn deduplication(self, kind: ClientEndpointNoticeKind) -> Deduplication {
        match (self, kind) {
            (Self::Command(_), ClientEndpointNoticeKind::Timeout) => Deduplication::UntilSuccess,
            (_, ClientEndpointNoticeKind::Rejected | ClientEndpointNoticeKind::Unavailable) => {
                Deduplication::VisibleBody
            }
            _ => Deduplication::None,
        }
    }

    pub(in crate::shell) fn automatic_body_row_limit(self) -> Option<usize> {
        (self != Self::MachineDiagnostic).then_some(crate::limits::MAX_AUTOMATIC_NOTICE_BODY_ROWS)
    }

    fn is_boot_queued(self) -> bool {
        matches!(
            self,
            Self::SessionRestoreIncomplete | Self::SessionSavesStopped
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::shell) struct ClientEndpointNoticeKey {
    /// Machine notices can share a boot id and code across hosts, so endpoint
    /// identity is kept as a typed part of the key.
    pub(in crate::shell) endpoint_id: Option<ClientEndpointId>,
    /// The server boot the notice is about; `None` for a notice no server
    /// boot raised (no snapshot yet, or a configured machine's diagnostic).
    pub(in crate::shell) boot_id: Option<shepr_protocol::BootId>,
    pub(in crate::shell) kind: ClientEndpointNoticeKind,
    pub(in crate::shell) code: NoticeCode,
}

pub(in crate::shell) struct ClientVisibleEndpointNotice {
    pub(in crate::shell) key: ClientEndpointNoticeKey,
    pub(in crate::shell) title: String,
    pub(in crate::shell) body: String,
}

#[derive(Default)]
pub(in crate::shell) struct Notices {
    timeout_seen: HashSet<ClientEndpointNoticeKey>,
    visible: Option<ClientVisibleEndpointNotice>,
    boot_seen: HashSet<ClientEndpointNoticeKey>,
    boot_queue: VecDeque<ClientVisibleEndpointNotice>,
    drawn_until: Option<std::time::Instant>,
}

impl Notices {
    pub(in crate::shell) fn visible(&self) -> Option<&ClientVisibleEndpointNotice> {
        self.visible.as_ref()
    }
    pub(in crate::shell) fn command_succeeded(
        &mut self,
        boot_id: &shepr_protocol::BootId,
        command: CommandKind,
    ) {
        self.timeout_seen.remove(&ClientEndpointNoticeKey {
            endpoint_id: None,
            boot_id: Some(boot_id.clone()),
            kind: ClientEndpointNoticeKind::Timeout,
            code: NoticeCode::Command(command),
        });
    }
    pub(in crate::shell) fn reset_endpoint(&mut self) {
        self.timeout_seen.clear();
        if !self
            .visible
            .as_ref()
            .is_some_and(|notice| self.boot_seen.contains(&notice.key))
        {
            self.advance();
        }
    }
    pub(in crate::shell) fn open_diagnostic(
        &mut self,
        notice: ClientVisibleEndpointNotice,
    ) -> bool {
        if self.visible.as_ref().is_none_or(|current| {
            current.key.endpoint_id == notice.key.endpoint_id && current.key.code == notice.key.code
        }) {
            self.drawn_until = None;
            self.visible = Some(notice);
            true
        } else {
            false
        }
    }
    pub(in crate::shell) fn push(
        &mut self,
        boot_id: Option<shepr_protocol::BootId>,
        kind: ClientEndpointNoticeKind,
        code: NoticeCode,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> bool {
        let key = ClientEndpointNoticeKey {
            endpoint_id: None,
            boot_id,
            kind,
            code,
        };
        let body = body.into();
        // Code selects the duplicate policy; a repeated visible notice never extends its
        // lifetime, while a timeout stays suppressed until that command later succeeds.
        match code.deduplication(kind) {
            Deduplication::VisibleBody => {
                if self
                    .visible
                    .as_ref()
                    .is_some_and(|notice| notice.key == key && notice.body == body)
                {
                    return false;
                }
            }
            Deduplication::UntilSuccess => {
                if !self.timeout_seen.insert(key.clone()) {
                    return false;
                }
            }
            Deduplication::None => {}
        }
        // A matching notice can return after the previous card was dismissed. Its next draw
        // starts a fresh lifetime instead of inheriting the hidden card's deadline.
        if self
            .visible
            .as_ref()
            .is_some_and(|notice| self.boot_seen.contains(&notice.key))
            && let Some(notice) = self.visible.take()
        {
            self.boot_queue.push_front(notice);
        }
        self.drawn_until = None;
        self.visible = Some(ClientVisibleEndpointNotice {
            key,
            title: title.into(),
            body,
        });
        true
    }
    pub(in crate::shell) fn queue_boot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        boot_id: &shepr_protocol::BootId,
        code: NoticeCode,
        title: &str,
        body: String,
    ) -> bool {
        let key = ClientEndpointNoticeKey {
            endpoint_id: Some(endpoint_id.clone()),
            boot_id: Some(boot_id.clone()),
            kind: ClientEndpointNoticeKind::Rejected,
            code,
        };
        assert!(code.is_boot_queued());
        if !self.boot_seen.insert(key.clone()) {
            return false;
        }
        let label = endpoint_id.display_label();
        self.boot_queue.push_back(ClientVisibleEndpointNotice {
            key,
            title: format!("{label}: {title}"),
            body,
        });
        if self.visible.is_none() {
            self.advance();
        }
        true
    }
    /// Starts the lifetime of the endpoint notice card, once per distinct notice, when a
    /// compose first draws it.
    pub(in crate::shell) fn drawn(&mut self, now: std::time::Instant) {
        if self.visible.is_some() && self.drawn_until.is_none() {
            self.drawn_until = Some(now + crate::limits::ENDPOINT_NOTICE_TIMEOUT);
        }
    }

    /// Hides the endpoint notice card once its lifetime (started when first drawn) runs
    /// out. A replacement notice carries its own lifetime, so it is not cut short by its
    /// predecessor's. Returns whether anything was hidden.
    pub(in crate::shell) fn tick(&mut self, now: std::time::Instant) -> bool {
        let notice_expired = self.deadline().is_some_and(|deadline| now >= deadline);
        if notice_expired {
            self.advance();
            return true;
        }

        false
    }

    /// Retire the visible card and show the next queued boot card, if any.
    pub(in crate::shell) fn advance(&mut self) {
        self.visible = self.boot_queue.pop_front();
        self.drawn_until = None;
    }

    pub(in crate::shell) fn deadline(&self) -> Option<std::time::Instant> {
        self.visible.as_ref()?;
        self.drawn_until
    }
    #[cfg(test)]
    pub(in crate::shell) fn timeout_suppressed(&self, code: NoticeCode) -> bool {
        self.timeout_seen
            .iter()
            .any(|key| key.kind == ClientEndpointNoticeKind::Timeout && key.code == code)
    }
    #[cfg(test)]
    pub(in crate::shell) fn queued(&self) -> usize {
        self.boot_queue.len()
    }
}
