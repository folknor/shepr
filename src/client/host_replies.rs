use crate::raw_input::{HostReplyPolicy, RawInputEvent, RawInputFramer};

const COLOR_QUERY_REPLIES: u16 = 258;

/// Client-side accounting for replies to queries sent to the outer terminal.
/// The byte framer only asks whether a reply may still be in flight.
#[derive(Default)]
pub(crate) struct HostReplies {
    color: u16,
    cell_size: bool,
    appearance: bool,
    track_color_scheme: bool,
    query_appearance_on_focus: bool,
}

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

impl HostReplyPolicy for HostReplies {
    fn color_query_sent(&mut self) {
        self.color = COLOR_QUERY_REPLIES;
    }

    fn cell_size_query_sent(&mut self) {
        self.cell_size = true;
    }

    fn enable_color_scheme_tracking(&mut self) {
        self.track_color_scheme = true;
    }

    fn enable_appearance_query_on_focus(&mut self) {
        self.query_appearance_on_focus = true;
    }

    fn awaiting_reply(&self) -> bool {
        self.color > 0 || self.cell_size || self.appearance
    }

    fn awaiting_cell_size_or_appearance(&self) -> bool {
        self.cell_size || self.appearance
    }

    fn awaiting_cell_size(&self) -> bool {
        self.cell_size
    }

    fn awaiting_appearance(&self) -> bool {
        self.appearance
    }

    fn clear_cell_size_and_appearance(&mut self) {
        self.cell_size = false;
        self.appearance = false;
    }

    fn clear_cell_size(&mut self) {
        self.cell_size = false;
    }

    fn clear_appearance(&mut self) {
        self.appearance = false;
    }

    fn clear_all(&mut self) {
        self.color = 0;
        self.cell_size = false;
        self.appearance = false;
    }

    fn observe(&mut self, event: &RawInputEvent) {
        match event {
            RawInputEvent::HostDefaultColor { .. } | RawInputEvent::HostPaletteColors { .. } => {
                self.color = self.color.saturating_sub(1);
            }
            RawInputEvent::HostCellSizeReport { .. } => self.cell_size = false,
            RawInputEvent::OuterFocusGained if self.query_appearance_on_focus => {
                self.appearance = true;
            }
            RawInputEvent::HostColorSchemeChanged(_) => {
                self.appearance = false;
                if self.track_color_scheme {
                    self.color_query_sent();
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_and_scheme_reports_update_only_armed_reply_windows() {
        let mut replies = HostReplies::default();
        replies.observe(&RawInputEvent::OuterFocusGained);
        assert!(!replies.awaiting_reply());

        replies.enable_appearance_query_on_focus();
        replies.observe(&RawInputEvent::OuterFocusGained);
        assert!(replies.awaiting_appearance());
        replies.observe(&RawInputEvent::HostColorSchemeChanged(
            crate::host_term::theme::HostAppearance::Dark,
        ));
        assert!(!replies.awaiting_reply());

        replies.enable_color_scheme_tracking();
        replies.observe(&RawInputEvent::HostColorSchemeChanged(
            crate::host_term::theme::HostAppearance::Dark,
        ));
        assert!(replies.awaiting_reply());
        replies.clear_all();
        assert!(!replies.awaiting_reply());
    }
}
