use super::*;
use crate::limits::{MAX_QUERY_BYTES, MAX_RETURNED_MATCHES};
use shepr_protocol::command::EndpointError;

/// The history origin a search certifies its global count against: the one
/// observed before the scan, when the scan reached the end of the text and no
/// row was evicted while it ran. Output that only rewrites rows keeps the
/// count, as it would right after the search; eviction or an early stop (a
/// re-wrap or screen switch between chunks) leaves the count unknown.
fn stable_search_history_origin(
    complete: bool,
    origin_before: Option<shepr_term::AbsRow>,
    origin_after: Option<shepr_term::AbsRow>,
) -> Option<shepr_term::AbsRow> {
    match (origin_before, origin_after) {
        (Some(origin), Some(after_origin)) if complete && origin == after_origin => Some(origin),
        _ => None,
    }
}

impl App {
    pub(crate) fn handle_pane_clear(&mut self, target: &PaneTarget) -> HandlerResult {
        let (_, pane_id) = self.endpoint_pane(&target.pane_id)?;
        let Some(runtime) = self.lookup_runtime(pane_id) else {
            return Err(pane_missing(&target.pane_id).into());
        };
        match runtime.clear_screen() {
            Ok(change) => Handled::done_with_effects(EndpointEffects::pane_viewers(
                pane_id,
                change.is_changed(),
            )),
            Err(shepr_mux::pane::PaneClearError::AlternateScreenActive) => {
                Err(EndpointError::AlternateScreen(target.pane_id).into())
            }
            Err(err) => Err(EndpointError::ResourceFailure(format!(
                "the pane could not be cleared: {err}"
            ))
            .into()),
        }
    }

    pub(crate) fn handle_pane_scroll(&mut self, params: &PaneScrollParams) -> HandlerResult {
        let (_, pane_id) = self.endpoint_pane(&params.pane_id)?;
        let Some(runtime) = self.lookup_runtime(pane_id) else {
            return Err(pane_missing(&params.pane_id).into());
        };
        let scroll_changed = runtime
            .set_scroll_offset_from_bottom(params.offset_from_bottom)
            .is_changed();
        let Some(pane) = self.pane_info(pane_id) else {
            return Err(HandlerError {
                error: pane_missing(&params.pane_id),
                effects: EndpointEffects::pane_viewers(pane_id, scroll_changed),
            });
        };
        Handled::reply_with_effects(
            EndpointReply::PaneInfo {
                pane: Box::new(pane),
            },
            EndpointEffects::pane_viewers(pane_id, scroll_changed),
        )
    }

    pub(crate) fn pane_selection_text(
        &self,
        params: &PaneSelectionReadParams,
    ) -> Result<String, EndpointError> {
        let (_, pane_id) = self.endpoint_pane(&params.pane_id)?;
        let Some(runtime) = self.lookup_runtime(pane_id) else {
            return Err(pane_missing(&params.pane_id));
        };
        let selection =
            shepr_vt::selection::Selection::range(pane_id, params.anchor, params.cursor);
        let Some(text) = runtime.read().extract_selection(&selection) else {
            return Err(EndpointError::Unavailable(
                "selection text is unavailable".to_owned(),
            ));
        };
        Ok(text)
    }

    pub(crate) fn handle_pane_selection_read(
        &mut self,
        params: PaneSelectionReadParams,
    ) -> HandlerResult {
        let text = self.pane_selection_text(&params)?;
        Handled::reply(EndpointReply::PaneSelection {
            pane_id: params.pane_id,
            text,
        })
    }

    pub(crate) fn handle_pane_copy_motion(
        &mut self,
        params: PaneCopyMotionParams,
    ) -> HandlerResult {
        let (_, pane_id) = self.endpoint_pane(&params.pane_id)?;
        let Some(runtime) = self.lookup_runtime(pane_id) else {
            return Err(pane_missing(&params.pane_id).into());
        };
        let target = match runtime.read().copy_motion(params.cursor, params.motion) {
            Ok(target) => target,
            Err(shepr_mux::pane::TerminalCopyMotionError::RowUnavailable) => {
                return Err(
                    EndpointError::Unavailable("terminal row is unavailable".into()).into(),
                );
            }
        };
        Handled::reply(EndpointReply::PaneCopyMotion {
            pane_id: params.pane_id,
            cursor: target,
        })
    }

    pub(crate) fn handle_pane_copy_search(
        &mut self,
        params: &PaneCopySearchParams,
    ) -> HandlerResult {
        let (_, pane_id) = self.endpoint_pane(&params.pane_id)?;
        let Some(runtime) = self.lookup_runtime(pane_id) else {
            return Err(pane_missing(&params.pane_id).into());
        };
        if params.query.len() > MAX_QUERY_BYTES {
            return Err(
                EndpointError::InvalidArgument("copy search query is too large".into()).into(),
            );
        }
        let previous = params
            .previous
            .map(|previous| shepr_mux::pane::TerminalTextRange {
                start: previous.start,
                end: previous.end,
            });
        let direction = params.direction;
        // The chunked terminal search releases its core lock between reads and
        // stops early if the screen shape or active screen changes. Only certify
        // the count when the scan completed under one history origin.
        let origin_before = runtime
            .read()
            .scroll_metrics()
            .map(|scroll| scroll.history_origin);
        let result = runtime
            .read()
            .search_text_window(shepr_mux::pane::TerminalTextSearch {
                query: &params.query,
                case: shepr_mux::pane::TerminalSearchCase::Smart,
                direction,
                cursor: params.cursor,
                previous,
                limit: shepr_mux::pane::TerminalSearchLimit::new(MAX_RETURNED_MATCHES),
            });
        let history_origin = stable_search_history_origin(
            result.complete,
            origin_before,
            runtime
                .read()
                .scroll_metrics()
                .map(|scroll| scroll.history_origin),
        );
        let matches = result
            .matches
            .into_iter()
            .map(|text_match| PaneTextRange {
                start: text_match.start,
                end: text_match.end,
            })
            .collect();
        Handled::reply(EndpointReply::PaneCopySearch {
            pane_id: params.pane_id,
            search: shepr_protocol::command::PaneCopySearch {
                history_origin,
                matches,
                total: result.total,
                current: result.current.map(|position| {
                    shepr_protocol::command::PaneCopySearchPosition {
                        window_index: position.window_index,
                        global_index: position.global_index,
                    }
                }),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::stable_search_history_origin;

    #[test]
    fn search_origin_requires_a_complete_scan_under_one_origin() {
        let origin = shepr_term::AbsRow(7);
        assert_eq!(
            stable_search_history_origin(true, Some(origin), Some(origin)),
            Some(origin)
        );
        // An early stop counted only part of the text.
        assert_eq!(
            stable_search_history_origin(false, Some(origin), Some(origin)),
            None
        );
        // Rows were evicted while the scan ran.
        assert_eq!(
            stable_search_history_origin(true, Some(origin), Some(shepr_term::AbsRow(8))),
            None
        );
        assert_eq!(stable_search_history_origin(true, None, Some(origin)), None);
    }
}
