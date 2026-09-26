use super::*;

impl App {
    pub(crate) fn handle_pane_clear(&mut self, id: String, target: &PaneTarget) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&target.pane_id) else {
            return pane_not_found(id, &target.pane_id);
        };
        let Some(runtime) =
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        else {
            return pane_not_found(id, &target.pane_id);
        };
        match runtime.clear_screen() {
            Ok(()) => encode_success(id, ResponseResult::Ok {}),
            Err(err) => encode_error(id, "pane_clear_failed", err),
        }
    }

    pub(crate) fn handle_pane_scroll(&mut self, id: String, params: &PaneScrollParams) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(runtime) =
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        else {
            return pane_not_found(id, &params.pane_id);
        };
        runtime.set_scroll_offset_from_bottom(
            usize::try_from(params.offset_from_bottom).unwrap_or(usize::MAX),
        );
        let Some(pane) = self.pane_info(ws_idx, pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        encode_success(id, ResponseResult::PaneInfo { pane })
    }

    pub(crate) fn pane_selection_text(
        &self,
        params: &PaneSelectionReadParams,
    ) -> Result<String, (&'static str, String)> {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return Err((
                "pane_not_found",
                format!("pane not found: {}", params.pane_id),
            ));
        };
        let Some(runtime) =
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        else {
            return Err((
                "pane_not_found",
                format!("pane not found: {}", params.pane_id),
            ));
        };
        let before = runtime.content_seq();
        if params
            .content_revision
            .is_some_and(|revision| revision != before || !before.is_multiple_of(2))
        {
            return Err(("stale_content", "pane content changed".to_owned()));
        }
        let selection = crate::selection::Selection::range(
            pane_id,
            crate::terminal::Point::new(params.anchor.row, params.anchor.col),
            crate::terminal::Point::new(params.cursor.row, params.cursor.col),
        );
        let Some(text) = runtime.extract_selection(&selection) else {
            return Err((
                "selection_unavailable",
                "selection text is unavailable".to_owned(),
            ));
        };
        if params.content_revision.is_some() && runtime.content_seq() != before {
            return Err(("stale_content", "pane content changed".to_owned()));
        }
        Ok(text)
    }

    pub(crate) fn handle_pane_selection_read(
        &mut self,
        id: String,
        params: PaneSelectionReadParams,
    ) -> String {
        match self.pane_selection_text(&params) {
            Ok(text) => encode_success(
                id,
                ResponseResult::PaneSelection {
                    pane_id: params.pane_id,
                    text,
                },
            ),
            Err((code, message)) => encode_error(id, code, message),
        }
    }

    pub(crate) fn handle_pane_copy_motion(
        &mut self,
        id: String,
        params: PaneCopyMotionParams,
    ) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(runtime) =
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        else {
            return pane_not_found(id, &params.pane_id);
        };
        let before = runtime.content_seq();
        if params
            .content_revision
            .is_some_and(|revision| revision != before || !before.is_multiple_of(2))
        {
            return encode_error(id, "stale_content", "pane content changed");
        }
        let origin = runtime
            .scroll_metrics()
            .map_or(crate::terminal::AbsRow(0), |metrics| metrics.history_origin);
        let absolute_cursor_row = params.cursor.row.absolute(origin);
        let target = match params.motion {
            PaneCopyMotion::LineEnd | PaneCopyMotion::FirstNonBlank => {
                let width = runtime
                    .terminal_dimensions()
                    .map_or(1, |(cols, _)| cols.max(1));
                let selection = crate::selection::Selection::range(
                    pane_id,
                    crate::terminal::Point::new(absolute_cursor_row, 0),
                    crate::terminal::Point::new(absolute_cursor_row, width.saturating_sub(1)),
                );
                let Some(text) = runtime.extract_selection(&selection) else {
                    return encode_error(
                        id,
                        "copy_motion_unavailable",
                        "terminal row is unavailable",
                    );
                };
                let col = if params.motion == PaneCopyMotion::LineEnd {
                    crate::copy_mode::last_character_col(&text).unwrap_or(0)
                } else {
                    crate::copy_mode::first_non_blank_col(&text).unwrap_or(0)
                };
                crate::pane::TerminalTextPoint {
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
                    return encode_error(
                        id,
                        "copy_motion_unavailable",
                        "copy motion is not a word motion",
                    );
                };
                runtime
                    .word_motion_target(params.cursor.row, params.cursor.col, motion)
                    .unwrap_or(crate::pane::TerminalTextPoint {
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
                .map(|target| crate::pane::TerminalTextPoint {
                    row: target.row,
                    col: params.cursor.col,
                })
                .unwrap_or(crate::pane::TerminalTextPoint {
                    row: params.cursor.row,
                    col: params.cursor.col,
                }),
        };
        let after = runtime.content_seq();
        if params.content_revision.is_some() && after != before {
            return encode_error(id, "stale_content", "pane content changed");
        }
        encode_success(
            id,
            ResponseResult::PaneCopyMotion {
                pane_id: params.pane_id,
                cursor: crate::api::schema::PaneTextPoint {
                    row: target.row,
                    col: target.col,
                },
                content_revision: after,
            },
        )
    }

    pub(crate) fn handle_pane_copy_search(
        &mut self,
        id: String,
        params: PaneCopySearchParams,
    ) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return pane_not_found(id, &params.pane_id);
        };
        let Some(runtime) =
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        else {
            return pane_not_found(id, &params.pane_id);
        };
        const MAX_QUERY_BYTES: usize = 4096;
        const MAX_RETURNED_MATCHES: usize = 1024;
        if params.query.len() > MAX_QUERY_BYTES {
            return encode_error(id, "query_too_large", "copy search query is too large");
        }
        let before = runtime.content_seq();
        if before != params.content_revision || !before.is_multiple_of(2) {
            return encode_error(id, "stale_content", "pane content changed");
        }
        let cursor = crate::pane::TerminalTextPoint {
            row: params.cursor.row,
            col: params.cursor.col,
        };
        let previous = params.previous.map(|previous| {
            (
                crate::pane::TerminalTextPoint {
                    row: previous.start.row,
                    col: previous.start.col,
                },
                crate::pane::TerminalTextPoint {
                    row: previous.end.row,
                    col: previous.end.col,
                },
            )
        });
        let direction = match params.direction {
            PaneCopySearchDirection::Forward => crate::pane::TerminalSearchDirection::Forward,
            PaneCopySearchDirection::Backward => crate::pane::TerminalSearchDirection::Backward,
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
            return encode_error(id, "stale_content", "pane content changed");
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
        encode_success(
            id,
            ResponseResult::PaneCopySearch {
                pane_id: params.pane_id,
                content_revision: after,
                matches,
                total: u64::try_from(result.total).unwrap_or(u64::MAX),
                current: result.current.and_then(|index| u32::try_from(index).ok()),
                current_global: result
                    .current_global
                    .and_then(|index| u64::try_from(index).ok()),
            },
        )
    }
}
