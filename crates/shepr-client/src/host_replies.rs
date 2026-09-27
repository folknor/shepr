use shepr_termio::input::raw_input::{HostReplies, RawInputFramer};

pub(super) struct HostInputFramer(RawInputFramer<HostReplies>);

impl HostInputFramer {
    pub(super) fn for_host_input() -> Self {
        Self(RawInputFramer::for_host_input())
    }
}

impl std::ops::Deref for HostInputFramer {
    type Target = RawInputFramer<HostReplies>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for HostInputFramer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_termio::input::raw_input::{HostReplyPolicy, RawInputEvent};

    #[test]
    fn focus_and_scheme_reports_update_only_armed_reply_windows() {
        let mut replies = HostReplies::default();
        replies.observe(&RawInputEvent::OuterFocusGained);
        assert!(!replies.awaiting_reply());

        replies.enable_appearance_query_on_focus();
        replies.observe(&RawInputEvent::OuterFocusGained);
        assert!(replies.awaiting_appearance());
        replies.observe(&RawInputEvent::HostColorSchemeChanged(
            shepr_termio::host_term::theme::HostAppearance::Dark,
        ));
        assert!(!replies.awaiting_reply());

        replies.enable_color_scheme_tracking();
        replies.observe(&RawInputEvent::HostColorSchemeChanged(
            shepr_termio::host_term::theme::HostAppearance::Dark,
        ));
        assert!(replies.awaiting_reply());
        replies.clear_all();
        assert!(!replies.awaiting_reply());
    }
}
