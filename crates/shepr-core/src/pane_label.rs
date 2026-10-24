use std::borrow::Cow;

/// The name a pane is shown under: the name the user gave it, the name its
/// agent was detected under (after the directory the agent runs in), or
/// both as `given (detected)` when they differ. `None` for a pane with
/// neither. The server's border titles and the client's lists share it.
pub fn pane_display_label<'a>(
    given: Option<&'a str>,
    detected: Option<&'a str>,
) -> Option<Cow<'a, str>> {
    match (given, detected) {
        (Some(given), Some(detected)) if given != detected => {
            Some(Cow::Owned(format!("{given} ({detected})")))
        }
        (Some(label), _) | (None, Some(label)) => Some(Cow::Borrowed(label)),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_given_name_leads_and_a_differing_detected_one_follows() {
        assert_eq!(pane_display_label(None, None), None);
        assert_eq!(
            pane_display_label(Some("build"), None).as_deref(),
            Some("build")
        );
        assert_eq!(
            pane_display_label(None, Some("pbfhogg")).as_deref(),
            Some("pbfhogg")
        );
        assert_eq!(
            pane_display_label(Some("review"), Some("pbfhogg")).as_deref(),
            Some("review (pbfhogg)")
        );
        assert_eq!(
            pane_display_label(Some("shepr"), Some("shepr")).as_deref(),
            Some("shepr")
        );
    }
}
