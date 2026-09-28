use super::*;

impl App {
    pub(crate) fn handle_pane_clear(&mut self, target: &PaneTarget) -> shepr_api::error::ApiResult {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&target.pane_id) else {
            return Err(pane_not_found(Some(&target.pane_id)));
        };
        let Some(runtime) =
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        else {
            return Err(pane_not_found(Some(&target.pane_id)));
        };
        match runtime.clear_screen() {
            Ok(()) => success(ResponseResult::Ok {}),
            Err(err) => failure(shepr_api::error::ApiErrorCode::PaneClearFailed, err),
        }
    }

    pub(crate) fn handle_pane_scroll(
        &mut self,
        params: &PaneScrollParams,
    ) -> shepr_api::error::ApiResult {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return Err(pane_not_found(Some(&params.pane_id)));
        };
        let Some(runtime) =
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        else {
            return Err(pane_not_found(Some(&params.pane_id)));
        };
        runtime.set_scroll_offset_from_bottom(
            usize::try_from(params.offset_from_bottom).unwrap_or(usize::MAX),
        );
        let Some(pane) = self.pane_info(ws_idx, pane_id) else {
            return Err(pane_not_found(Some(&params.pane_id)));
        };
        success(ResponseResult::PaneInfo { pane })
    }

    pub(crate) fn pane_selection_text(
        &self,
        params: &PaneSelectionReadParams,
    ) -> Result<String, shepr_api::error::ApiError> {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return Err(pane_not_found(Some(&params.pane_id)));
        };
        let Some(runtime) =
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        else {
            return Err(pane_not_found(Some(&params.pane_id)));
        };
        let before = runtime.content_seq();
        if params
            .content_revision
            .is_some_and(|revision| revision != before || !before.is_multiple_of(2))
        {
            return Err(shepr_api::error::ApiError::new(
                shepr_api::error::ApiErrorCode::StaleContent,
                "pane content changed",
            ));
        }
        let selection = shepr_vt::selection::Selection::range(
            pane_id,
            shepr_vt::Point::new(params.anchor.row, params.anchor.col),
            shepr_vt::Point::new(params.cursor.row, params.cursor.col),
        );
        let Some(text) = runtime.extract_selection(&selection) else {
            return Err(shepr_api::error::ApiError::new(
                shepr_api::error::ApiErrorCode::SelectionUnavailable,
                "selection text is unavailable",
            ));
        };
        if params.content_revision.is_some() && runtime.content_seq() != before {
            return Err(shepr_api::error::ApiError::new(
                shepr_api::error::ApiErrorCode::StaleContent,
                "pane content changed",
            ));
        }
        Ok(text)
    }

    pub(crate) fn handle_pane_selection_read(
        &mut self,
        params: PaneSelectionReadParams,
    ) -> shepr_api::error::ApiResult {
        match self.pane_selection_text(&params) {
            Ok(text) => success(ResponseResult::PaneSelection {
                pane_id: params.pane_id,
                text,
            }),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn handle_pane_copy_motion(
        &mut self,
        params: PaneCopyMotionParams,
    ) -> shepr_api::error::ApiResult {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return Err(pane_not_found(Some(&params.pane_id)));
        };
        let Some(runtime) =
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        else {
            return Err(pane_not_found(Some(&params.pane_id)));
        };
        let before = runtime.content_seq();
        if params
            .content_revision
            .is_some_and(|revision| revision != before || !before.is_multiple_of(2))
        {
            return failure(
                shepr_api::error::ApiErrorCode::StaleContent,
                "pane content changed",
            );
        }
        let origin = runtime
            .scroll_metrics()
            .map_or(shepr_vt::AbsRow(0), |metrics| metrics.history_origin);
        let absolute_cursor_row = params.cursor.row.absolute(origin);
        let target = match params.motion {
            PaneCopyMotion::LineEnd | PaneCopyMotion::FirstNonBlank => {
                let width = runtime
                    .terminal_dimensions()
                    .map_or(1, |(cols, _)| cols.max(1));
                let selection = shepr_vt::selection::Selection::range(
                    pane_id,
                    shepr_vt::Point::new(absolute_cursor_row, 0),
                    shepr_vt::Point::new(absolute_cursor_row, width.saturating_sub(1)),
                );
                let Some(text) = runtime.extract_selection(&selection) else {
                    return failure(
                        shepr_api::error::ApiErrorCode::CopyMotionUnavailable,
                        "terminal row is unavailable",
                    );
                };
                let col = if params.motion == PaneCopyMotion::LineEnd {
                    shepr_termio::copy_mode::last_character_col(&text).unwrap_or(0)
                } else {
                    shepr_termio::copy_mode::first_non_blank_col(&text).unwrap_or(0)
                };
                shepr_mux::pane::TerminalTextPoint {
                    row: params.cursor.row,
                    col: col.min(width.saturating_sub(1)),
                }
            }
            PaneCopyMotion::NextWordStart
            | PaneCopyMotion::PreviousWordStart
            | PaneCopyMotion::NextWordEnd
            | PaneCopyMotion::NextBigWordStart
            | PaneCopyMotion::PreviousBigWordStart
            | PaneCopyMotion::NextBigWordEnd => {
                let Some(motion) = terminal_word_motion(params.motion) else {
                    return failure(
                        shepr_api::error::ApiErrorCode::CopyMotionUnavailable,
                        "copy motion is not a word motion",
                    );
                };
                runtime
                    .word_motion_target(params.cursor.row, params.cursor.col, motion)
                    .unwrap_or(shepr_mux::pane::TerminalTextPoint {
                        row: params.cursor.row,
                        col: params.cursor.col,
                    })
            }
            PaneCopyMotion::PreviousParagraph | PaneCopyMotion::NextParagraph => runtime
                .paragraph_motion_target(
                    params.cursor.row,
                    if params.motion == PaneCopyMotion::PreviousParagraph {
                        -1
                    } else {
                        1
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
        let after = runtime.content_seq();
        if params.content_revision.is_some() && after != before {
            return failure(
                shepr_api::error::ApiErrorCode::StaleContent,
                "pane content changed",
            );
        }
        success(ResponseResult::PaneCopyMotion {
            pane_id: params.pane_id,
            cursor: shepr_api::schema::PaneTextPoint {
                row: target.row,
                col: target.col,
            },
            content_revision: after,
        })
    }

    pub(crate) fn handle_pane_copy_search(
        &mut self,
        params: PaneCopySearchParams,
    ) -> shepr_api::error::ApiResult {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return Err(pane_not_found(Some(&params.pane_id)));
        };
        let Some(runtime) =
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        else {
            return Err(pane_not_found(Some(&params.pane_id)));
        };
        const MAX_QUERY_BYTES: usize = 4096;
        const MAX_RETURNED_MATCHES: usize = 1024;
        if params.query.len() > MAX_QUERY_BYTES {
            return failure(
                shepr_api::error::ApiErrorCode::QueryTooLarge,
                "copy search query is too large",
            );
        }
        let before = runtime.content_seq();
        if before != params.content_revision || !before.is_multiple_of(2) {
            return failure(
                shepr_api::error::ApiErrorCode::StaleContent,
                "pane content changed",
            );
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
        let after = runtime.content_seq();
        if after != before || !after.is_multiple_of(2) {
            return failure(
                shepr_api::error::ApiErrorCode::StaleContent,
                "pane content changed",
            );
        }
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
        success(ResponseResult::PaneCopySearch {
            pane_id: params.pane_id,
            content_revision: after,
            matches,
            total: u64::try_from(result.total).unwrap_or(u64::MAX),
            current: result.current.and_then(|index| u32::try_from(index).ok()),
            current_global: result
                .current_global
                .and_then(|index| u64::try_from(index).ok()),
        })
    }
}
