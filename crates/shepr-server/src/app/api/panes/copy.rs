use super::*;
use crate::limits::{MAX_QUERY_BYTES, MAX_RETURNED_MATCHES};
use shepr_protocol::command::EndpointError;

impl App {
    pub(crate) fn handle_pane_clear(&mut self, target: &PaneTarget) -> HandlerResult {
        let (ws_idx, pane_id) = self.endpoint_pane(&target.pane_id)?;
        let Some(runtime) =
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        else {
            return Err(pane_missing(&target.pane_id).into());
        };
        match runtime.clear_screen() {
            Ok(change) => Handled::done_with_effects(EndpointEffects::pane_viewers(
                pane_id,
                change.is_changed(),
            )),
            Err(shepr_mux::pane::PaneClearError::AlternateScreenActive) => {
                rejected("the pane is on the alternate screen")
            }
            Err(err) => rejected(format!("the pane could not be cleared: {err}")),
        }
    }

    pub(crate) fn handle_pane_scroll(&mut self, params: &PaneScrollParams) -> HandlerResult {
        let (ws_idx, pane_id) = self.endpoint_pane(&params.pane_id)?;
        let Some(runtime) =
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        else {
            return Err(pane_missing(&params.pane_id).into());
        };
        let scroll_changed = runtime
            .set_scroll_offset_from_bottom(params.offset_from_bottom)
            .is_changed();
        let Some(pane) = self.pane_info(ws_idx, pane_id) else {
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
        let (ws_idx, pane_id) = self.endpoint_pane(&params.pane_id)?;
        let Some(runtime) =
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        else {
            return Err(pane_missing(&params.pane_id));
        };
        let selection =
            shepr_vt::selection::Selection::range(pane_id, params.anchor, params.cursor);
        let Some(text) = runtime.read().extract_selection(&selection) else {
            return Err(EndpointError::Rejected(
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
        let (ws_idx, pane_id) = self.endpoint_pane(&params.pane_id)?;
        let Some(runtime) =
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        else {
            return Err(pane_missing(&params.pane_id).into());
        };
        use shepr_mux::pane::{TerminalCopyMotion, TerminalLineMotion, TerminalParagraphMotion};
        let motion = match params.motion {
            PaneCopyMotion::Line(PaneLineMotion::End) => {
                TerminalCopyMotion::Line(TerminalLineMotion::End)
            }
            PaneCopyMotion::Line(PaneLineMotion::FirstNonBlank) => {
                TerminalCopyMotion::Line(TerminalLineMotion::FirstNonBlank)
            }
            PaneCopyMotion::Word(motion) => TerminalCopyMotion::Word(terminal_word_motion(motion)),
            PaneCopyMotion::Paragraph(PaneParagraphMotion::Previous) => {
                TerminalCopyMotion::Paragraph(TerminalParagraphMotion::Previous)
            }
            PaneCopyMotion::Paragraph(PaneParagraphMotion::Next) => {
                TerminalCopyMotion::Paragraph(TerminalParagraphMotion::Next)
            }
        };
        let target = match runtime.read().copy_motion(params.cursor, motion) {
            Ok(target) => target,
            Err(shepr_mux::pane::TerminalCopyMotionError::RowUnavailable) => {
                return rejected("terminal row is unavailable");
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
        let (ws_idx, pane_id) = self.endpoint_pane(&params.pane_id)?;
        let Some(runtime) =
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        else {
            return Err(pane_missing(&params.pane_id).into());
        };
        if params.query.len() > MAX_QUERY_BYTES {
            return rejected("copy search query is too large");
        }
        let previous = params
            .previous
            .map(|previous| shepr_mux::pane::TerminalTextRange {
                start: previous.start,
                end: previous.end,
            });
        let direction = match params.direction {
            PaneCopySearchDirection::Forward => shepr_mux::pane::TerminalSearchDirection::Forward,
            PaneCopySearchDirection::Backward => shepr_mux::pane::TerminalSearchDirection::Backward,
        };
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
