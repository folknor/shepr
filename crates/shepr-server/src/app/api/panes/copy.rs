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
            Ok(change) => Handled::done_with_effects(EndpointEffects {
                pane_surface_changed: change.is_changed(),
                ..EndpointEffects::default()
            }),
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
                effects: EndpointEffects {
                    pane_surface_changed: scroll_changed,
                    ..EndpointEffects::default()
                },
            });
        };
        Handled::reply_with_effects(
            EndpointReply::PaneInfo {
                pane: Box::new(pane),
            },
            EndpointEffects {
                pane_surface_changed: scroll_changed,
                ..EndpointEffects::default()
            },
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
        let selection = shepr_vt::selection::Selection::range(
            pane_id,
            shepr_vt::Point::new(params.anchor.row, params.anchor.col),
            shepr_vt::Point::new(params.cursor.row, params.cursor.col),
        );
        let Some(text) = runtime.extract_selection(&selection) else {
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
        let target = match params.motion {
            PaneCopyMotion::Line(motion) => {
                let width = runtime
                    .terminal_dimensions()
                    .map_or(1, |(cols, _)| cols.max(1));
                let selection = shepr_vt::selection::Selection::line_range(
                    pane_id,
                    params.cursor.row,
                    params.cursor.row,
                );
                let Some(text) = runtime.extract_selection(&selection) else {
                    return rejected("terminal row is unavailable");
                };
                let col = match motion {
                    PaneLineMotion::End => {
                        shepr_termio::copy_mode::last_character_col(&text).unwrap_or(0)
                    }
                    PaneLineMotion::FirstNonBlank => {
                        shepr_termio::copy_mode::first_non_blank_col(&text).unwrap_or(0)
                    }
                };
                shepr_mux::pane::TerminalTextPoint {
                    row: params.cursor.row,
                    col: col.min(width.saturating_sub(1)),
                }
            }
            PaneCopyMotion::Word(motion) => runtime
                .word_motion_target(
                    params.cursor.row,
                    params.cursor.col,
                    terminal_word_motion(motion),
                )
                .unwrap_or(shepr_mux::pane::TerminalTextPoint {
                    row: params.cursor.row,
                    col: params.cursor.col,
                }),
            PaneCopyMotion::Paragraph(motion) => runtime
                .paragraph_motion_target(
                    params.cursor.row,
                    match motion {
                        PaneParagraphMotion::Previous => -1,
                        PaneParagraphMotion::Next => 1,
                    },
                )
                .map_or(
                    shepr_mux::pane::TerminalTextPoint {
                        row: params.cursor.row,
                        col: params.cursor.col,
                    },
                    |target| shepr_mux::pane::TerminalTextPoint {
                        row: target.row,
                        col: params.cursor.col,
                    },
                ),
        };
        Handled::reply(EndpointReply::PaneCopyMotion {
            pane_id: params.pane_id,
            cursor: PaneTextPoint {
                row: target.row,
                col: target.col,
            },
        })
    }

    pub(crate) fn handle_pane_copy_search(
        &mut self,
        params: PaneCopySearchParams,
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
        let cursor = shepr_mux::pane::TerminalTextPoint {
            row: params.cursor.row,
            col: params.cursor.col,
        };
        let previous = params.previous.map(|previous| {
            (
                shepr_mux::pane::TerminalTextPoint {
                    row: previous.start.row,
                    col: previous.start.col,
                },
                shepr_mux::pane::TerminalTextPoint {
                    row: previous.end.row,
                    col: previous.end.col,
                },
            )
        });
        let direction = match params.direction {
            PaneCopySearchDirection::Forward => shepr_mux::pane::TerminalSearchDirection::Forward,
            PaneCopySearchDirection::Backward => shepr_mux::pane::TerminalSearchDirection::Backward,
        };
        let result = runtime.search_text_window(
            &params.query,
            params.query.chars().any(char::is_uppercase),
            direction,
            cursor,
            previous,
            MAX_RETURNED_MATCHES,
        );
        let matches = result
            .matches
            .into_iter()
            .map(|text_match| PaneTextRange {
                start: PaneTextPoint {
                    row: text_match.start.row,
                    col: text_match.start.col,
                },
                end: PaneTextPoint {
                    row: text_match.end.row,
                    col: text_match.end.col,
                },
            })
            .collect();
        Handled::reply(EndpointReply::PaneCopySearch {
            pane_id: params.pane_id,
            matches,
            total: u64::try_from(result.total).unwrap_or(u64::MAX),
            current: result.current.and_then(|index| u32::try_from(index).ok()),
            current_global: result
                .current_global
                .and_then(|index| u64::try_from(index).ok()),
        })
    }
}
