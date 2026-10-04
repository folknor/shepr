//! Notice suppression, boot-card queueing and drawn lifetimes have one owner.

use crate::endpoint::ClientEndpointId;
use crate::limits::{ENDPOINT_NOTICE_TIMEOUT, MAX_AUTOMATIC_NOTICE_BODY_ROWS};
use shepr_protocol::command::CommandKind;
use std::collections::{HashSet, VecDeque};

pub(in crate::shell) mod cards;
pub(in crate::shell) mod machine_diagnostics;
pub(in crate::shell) mod transient_error;

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
    Server,
    Cancelled,
    Command(CommandKind),
    Boot(BootNoticeCode),
    PaneInputDropped,
    OversizedSurface,
    SizeLimit,
    MachineDiagnostic,
    EndpointUnavailable,
}

/// The cards an endpoint's snapshot carries for its whole server boot. Only these queue
/// behind the visible card, once per endpoint, boot and code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::shell) enum BootNoticeCode {
    SessionRestoreIncomplete,
    SessionSavesStopped,
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

    fn automatic_body_row_limit(self) -> Option<usize> {
        (self != Self::MachineDiagnostic).then_some(MAX_AUTOMATIC_NOTICE_BODY_ROWS)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::shell) struct ClientEndpointNoticeKey {
    /// Machine notices can share a boot id and code across hosts, so endpoint
    /// identity is kept as a typed part of the key.
    endpoint_id: Option<ClientEndpointId>,
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
    fn open_diagnostic(&mut self, notice: ClientVisibleEndpointNotice) -> bool {
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
    /// Queues `endpoint_id`'s boot card, titled after `label`, the name the
    /// client shows for that endpoint.
    pub(in crate::shell) fn queue_boot(
        &mut self,
        endpoint_id: &ClientEndpointId,
        label: &str,
        boot_id: &shepr_protocol::BootId,
        code: BootNoticeCode,
        title: &str,
        body: String,
    ) -> bool {
        let key = ClientEndpointNoticeKey {
            endpoint_id: Some(endpoint_id.clone()),
            boot_id: Some(boot_id.clone()),
            kind: ClientEndpointNoticeKind::Rejected,
            code: NoticeCode::Boot(code),
        };
        if !self.boot_seen.insert(key.clone()) {
            return false;
        }
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
            self.drawn_until = Some(now + ENDPOINT_NOTICE_TIMEOUT);
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

#[cfg(test)]
impl crate::shell::ClientShellState {
    /// The title of the notice card on screen, for tests outside the shell.
    pub(crate) fn visible_notice_title(&self) -> Option<&str> {
        self.notices.visible().map(|notice| notice.title.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn boot(name: &str) -> shepr_protocol::BootId {
        crate::tests::test_boot_id(name)
    }

    fn visible_title(notices: &Notices) -> Option<&str> {
        notices.visible().map(|notice| notice.title.as_str())
    }

    fn push_rejected(notices: &mut Notices, title: &str, body: &str) -> bool {
        notices.push(
            Some(boot("boot")),
            ClientEndpointNoticeKind::Rejected,
            NoticeCode::Server,
            title,
            body,
        )
    }

    fn push_timeout(notices: &mut Notices, boot_id: &str, command: CommandKind) -> bool {
        notices.push(
            Some(boot(boot_id)),
            ClientEndpointNoticeKind::Timeout,
            NoticeCode::Command(command),
            "Server timed out",
            "no answer",
        )
    }

    fn queue_restore(notices: &mut Notices, boot_id: &str, title: &str) -> bool {
        notices.queue_boot(
            &ClientEndpointId::Local,
            "Desk",
            &boot(boot_id),
            BootNoticeCode::SessionRestoreIncomplete,
            title,
            "some panes were not restored".into(),
        )
    }

    #[test]
    fn a_repeated_visible_notice_is_dropped_but_a_new_body_replaces_it() {
        let mut notices = Notices::default();
        assert!(push_rejected(&mut notices, "Rejected", "first"));
        assert!(!push_rejected(&mut notices, "Rejected", "first"));
        assert!(push_rejected(&mut notices, "Rejected", "second"));
        assert_eq!(
            notices.visible().map(|notice| notice.body.as_str()),
            Some("second")
        );
        // Once the card is gone, the same notice may show again.
        notices.advance();
        assert!(notices.visible().is_none());
        assert!(push_rejected(&mut notices, "Rejected", "second"));
    }

    #[test]
    fn a_notice_without_a_duplicate_policy_shows_every_time() {
        let mut notices = Notices::default();
        for _ in 0..2 {
            assert!(notices.push(
                None,
                ClientEndpointNoticeKind::Timeout,
                NoticeCode::Server,
                "Server timed out",
                "no answer",
            ));
        }
    }

    #[test]
    fn a_command_timeout_is_suppressed_until_that_command_succeeds_on_that_boot() {
        let mut notices = Notices::default();
        let rename = CommandKind::WorkspaceRename;
        assert!(push_timeout(&mut notices, "boot", rename));
        notices.advance();
        // Suppressed even with no card on screen.
        assert!(!push_timeout(&mut notices, "boot", rename));
        assert!(notices.timeout_suppressed(NoticeCode::Command(rename)));
        // Another command, or the same one on another boot, is a different notice.
        assert!(push_timeout(
            &mut notices,
            "boot",
            CommandKind::PaneCopySearch
        ));
        assert!(push_timeout(&mut notices, "other-boot", rename));
        // Success on another boot leaves this boot's suppression in place.
        notices.command_succeeded(&boot("other-boot"), rename);
        assert!(!push_timeout(&mut notices, "boot", rename));
        notices.command_succeeded(&boot("boot"), rename);
        assert!(push_timeout(&mut notices, "boot", rename));
    }

    #[test]
    fn resetting_the_endpoint_clears_timeouts_and_retires_only_an_ordinary_card() {
        let mut notices = Notices::default();
        let rename = CommandKind::WorkspaceRename;
        assert!(push_timeout(&mut notices, "boot", rename));
        notices.reset_endpoint();
        assert!(notices.visible().is_none());
        assert!(!notices.timeout_suppressed(NoticeCode::Command(rename)));
        assert!(push_timeout(&mut notices, "boot", rename));

        // A boot card belongs to its server boot, not to the presentation: it stays.
        notices.advance();
        assert!(queue_restore(&mut notices, "boot", "Restore incomplete"));
        notices.reset_endpoint();
        assert_eq!(visible_title(&notices), Some("Desk: Restore incomplete"));
    }

    #[test]
    fn boot_cards_show_once_each_in_arrival_order() {
        let mut notices = Notices::default();
        assert!(queue_restore(&mut notices, "boot-1", "First"));
        // With nothing on screen the first card shows at once, named for its endpoint.
        assert_eq!(visible_title(&notices), Some("Desk: First"));
        assert_eq!(notices.queued(), 0);
        assert!(queue_restore(&mut notices, "boot-2", "Second"));
        assert_eq!(visible_title(&notices), Some("Desk: First"));
        assert_eq!(notices.queued(), 1);
        // The same boot's card is never queued twice, even after it was seen.
        assert!(!queue_restore(&mut notices, "boot-1", "First"));
        notices.advance();
        assert_eq!(visible_title(&notices), Some("Desk: Second"));
        notices.advance();
        assert!(notices.visible().is_none());
        assert!(!queue_restore(&mut notices, "boot-1", "First"));
        assert!(notices.visible().is_none());
    }

    #[test]
    fn a_notice_pushed_over_a_boot_card_puts_the_card_back_first_in_line() {
        let mut notices = Notices::default();
        assert!(queue_restore(&mut notices, "boot-1", "First"));
        assert!(queue_restore(&mut notices, "boot-2", "Second"));
        assert!(push_rejected(&mut notices, "Rejected", "body"));
        assert_eq!(visible_title(&notices), Some("Rejected"));
        assert_eq!(notices.queued(), 2);
        notices.advance();
        assert_eq!(visible_title(&notices), Some("Desk: First"));
        notices.advance();
        assert_eq!(visible_title(&notices), Some("Desk: Second"));
    }

    #[test]
    fn a_card_lives_from_its_first_draw_and_a_replacement_starts_its_own_lifetime() {
        let mut notices = Notices::default();
        let start = Instant::now();
        // Nothing on screen: drawing starts no lifetime.
        notices.drawn(start);
        assert!(notices.deadline().is_none());

        assert!(push_rejected(&mut notices, "Rejected", "first"));
        assert!(notices.deadline().is_none());
        assert!(!notices.tick(start + ENDPOINT_NOTICE_TIMEOUT * 4));
        notices.drawn(start);
        assert_eq!(notices.deadline(), Some(start + ENDPOINT_NOTICE_TIMEOUT));
        // A later draw does not extend it.
        notices.drawn(start + Duration::from_secs(1));
        assert_eq!(notices.deadline(), Some(start + ENDPOINT_NOTICE_TIMEOUT));

        // A replacement waits for its own first draw.
        let later = start + Duration::from_secs(1);
        assert!(push_rejected(&mut notices, "Rejected", "second"));
        assert!(notices.deadline().is_none());
        notices.drawn(later);
        assert_eq!(notices.deadline(), Some(later + ENDPOINT_NOTICE_TIMEOUT));
        assert!(!notices.tick(start + ENDPOINT_NOTICE_TIMEOUT));
        assert!(notices.tick(later + ENDPOINT_NOTICE_TIMEOUT));
        assert!(notices.visible().is_none());
        assert!(notices.deadline().is_none());
        assert!(!notices.tick(later + ENDPOINT_NOTICE_TIMEOUT * 2));
    }

    #[test]
    fn an_expired_card_gives_way_to_the_next_boot_card_with_a_fresh_lifetime() {
        let mut notices = Notices::default();
        let start = Instant::now();
        assert!(queue_restore(&mut notices, "boot-1", "First"));
        assert!(queue_restore(&mut notices, "boot-2", "Second"));
        notices.drawn(start);
        assert!(notices.tick(start + ENDPOINT_NOTICE_TIMEOUT));
        assert_eq!(visible_title(&notices), Some("Desk: Second"));
        assert!(notices.deadline().is_none());
    }
}
