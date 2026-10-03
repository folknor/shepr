//! Scroll positions and the scrollbar geometry the server draws and the
//! client hit-tests.

use crate::{AbsRow, ViewportRow};

/// A bottom-based history viewport. Construction clamps the offset to retained history.
/// The read-only fields are exposed through Deref; there is no mutable field access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "ScrollMetricsFields", into = "ScrollMetricsFields")]
pub struct ScrollMetrics(ScrollMetricsFields);

/// Read-only observations and the serde payload for a history viewport.
/// Converting this payload into ScrollMetrics validates the offset bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScrollMetricsFields {
    pub offset_from_bottom: usize,
    pub max_offset_from_bottom: usize,
    pub viewport_rows: usize,
    pub history_origin: AbsRow,
}

impl std::ops::Deref for ScrollMetrics {
    type Target = ScrollMetricsFields;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<ScrollMetrics> for ScrollMetricsFields {
    fn from(metrics: ScrollMetrics) -> Self {
        metrics.0
    }
}

impl TryFrom<ScrollMetricsFields> for ScrollMetrics {
    type Error = &'static str;
    fn try_from(fields: ScrollMetricsFields) -> Result<Self, Self::Error> {
        if fields.offset_from_bottom > fields.max_offset_from_bottom {
            return Err("scroll offset exceeds retained history");
        }
        Ok(Self(fields))
    }
}

impl ScrollMetrics {
    pub fn new(
        offset_from_bottom: usize,
        max_offset_from_bottom: usize,
        viewport_rows: usize,
        history_origin: AbsRow,
    ) -> Self {
        Self(ScrollMetricsFields {
            offset_from_bottom: offset_from_bottom.min(max_offset_from_bottom),
            max_offset_from_bottom,
            viewport_rows,
            history_origin,
        })
    }

    pub fn with_offset(self, offset_from_bottom: usize) -> Self {
        Self::new(
            offset_from_bottom,
            self.max_offset_from_bottom,
            self.viewport_rows,
            self.history_origin,
        )
    }

    /// Retained-buffer row at the top, counted from the oldest retained row.
    pub fn viewport_start(self) -> usize {
        self.max_offset_from_bottom - self.offset_from_bottom
    }

    pub fn viewport_top_row(self) -> AbsRow {
        self.history_origin
            .saturating_add(u64::try_from(self.viewport_start()).unwrap_or(u64::MAX))
    }

    pub fn absolute_row_at_viewport(self, row: ViewportRow) -> AbsRow {
        AbsRow::from_viewport_top(self.viewport_top_row(), row)
    }
}

/// Top-based scrolling for chrome lists; it carries no terminal history identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListScroll {
    start: usize,
    max_start: usize,
    viewport_rows: usize,
}

impl ListScroll {
    pub fn new(start: usize, max_start: usize, viewport_rows: usize) -> Self {
        Self {
            start: start.min(max_start),
            max_start,
            viewport_rows,
        }
    }

    pub fn start(self) -> usize {
        self.start
    }

    pub fn max_start(self) -> usize {
        self.max_start
    }

    pub fn viewport_rows(self) -> usize {
        self.viewport_rows
    }
}

/// Geometry shared by list and terminal scrollbar drawing and hit testing.
pub trait ScrollbarMetrics: Copy {
    fn start(self) -> usize;
    fn max_start(self) -> usize;
    fn viewport_rows(self) -> usize;
}

impl ScrollbarMetrics for ListScroll {
    fn start(self) -> usize {
        self.start
    }
    fn max_start(self) -> usize {
        self.max_start
    }
    fn viewport_rows(self) -> usize {
        self.viewport_rows
    }
}

impl ScrollbarMetrics for ScrollMetrics {
    fn start(self) -> usize {
        self.viewport_start()
    }
    fn max_start(self) -> usize {
        self.max_offset_from_bottom
    }
    fn viewport_rows(self) -> usize {
        self.viewport_rows
    }
}

/// The rows a vertical scrollbar occupies: its top screen row and its height.
/// Construction clamps the height so the far edge fits in u16.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollTrack {
    y: u16,
    height: u16,
}

impl ScrollTrack {
    pub const fn new(y: u16, height: u16) -> Self {
        Self {
            y,
            height: y.saturating_add(height) - y,
        }
    }

    pub const fn y(self) -> u16 {
        self.y
    }

    pub const fn height(self) -> u16 {
        self.height
    }

    /// The row just past the track; never wraps.
    pub const fn bottom(self) -> u16 {
        self.y + self.height
    }
}

/// What a scrollbar paints in one row of its track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarPart {
    Track,
    Thumb,
}

/// Every row of the track, top to bottom, with the part painted there.
/// Yields nothing when there is no thumb to show, so nothing is painted.
pub fn scrollbar_rows(
    metrics: impl ScrollbarMetrics,
    track: ScrollTrack,
) -> impl Iterator<Item = (u16, ScrollbarPart)> {
    let thumb = scrollbar_thumb(metrics, track);
    let rows = if thumb.is_some() {
        track.y..track.bottom()
    } else {
        0..0
    };
    rows.map(move |row| {
        let part = match thumb {
            Some(thumb) if row >= thumb.top && row - thumb.top < thumb.len => ScrollbarPart::Thumb,
            _ => ScrollbarPart::Track,
        };
        (row, part)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarThumb {
    pub top: u16,
    pub len: u16,
}

pub fn scrollbar_thumb(
    metrics: impl ScrollbarMetrics,
    track: ScrollTrack,
) -> Option<ScrollbarThumb> {
    if metrics.max_start() == 0 || track.height == 0 {
        return None;
    }

    let track_height = track.height as usize;
    let total_rows = metrics.max_start().saturating_add(metrics.viewport_rows());
    if total_rows == 0 {
        return None;
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the ratio is clamped to [1.0, track_height] just below, so the rounded result always fits in usize without truncation or sign loss"
    )]
    let thumb_len = (metrics.viewport_rows() as f32 * track_height as f32 / total_rows as f32)
        .round()
        .max(1.0)
        .min(track_height as f32) as usize;
    let max_thumb_top = track_height.saturating_sub(thumb_len);
    let scrolled_from_top = metrics.start();
    let thumb_top = if max_thumb_top == 0 || metrics.max_start() == 0 {
        0
    } else {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to [0.0, max_thumb_top] just above, so this always fits in usize"
        )]
        let clamped = (scrolled_from_top as f32 * max_thumb_top as f32 / metrics.max_start() as f32)
            .round()
            .clamp(0.0, max_thumb_top as f32) as usize;
        clamped
    };

    #[expect(
        clippy::cast_possible_truncation,
        reason = "thumb_len and thumb_top are bounded by track_height above, and ScrollTrack keeps track.y + track_height within u16, so these narrow and add without loss"
    )]
    let thumb = ScrollbarThumb {
        top: track.y + thumb_top as u16,
        len: thumb_len as u16,
    };
    Some(thumb)
}

pub fn scrollbar_thumb_grab_offset(
    metrics: impl ScrollbarMetrics,
    track: ScrollTrack,
    row: u16,
) -> Option<u16> {
    let thumb = scrollbar_thumb(metrics, track)?;
    (row >= thumb.top && row - thumb.top < thumb.len).then(|| row - thumb.top)
}

fn scrollbar_start_from_thumb_top(
    metrics: impl ScrollbarMetrics,
    track: ScrollTrack,
    thumb_top: usize,
) -> usize {
    if metrics.max_start() == 0 {
        return 0;
    }

    let thumb_len = scrollbar_thumb(metrics, track).map_or(1, |thumb| thumb.len as usize);
    let max_thumb_top = track.height as usize - thumb_len.min(track.height as usize);
    if max_thumb_top == 0 {
        return 0;
    }

    let desired_top = thumb_top.min(max_thumb_top);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "desired_top / max_thumb_top <= 1, so the scaled result stays within metrics.max_start() and fits in usize"
    )]
    let scrolled_from_top =
        (desired_top as f32 * metrics.max_start() as f32 / max_thumb_top as f32).round() as usize;
    scrolled_from_top.min(metrics.max_start())
}

pub fn scrollbar_start_from_row(
    metrics: impl ScrollbarMetrics,
    track: ScrollTrack,
    row: u16,
) -> usize {
    let thumb = match scrollbar_thumb(metrics, track) {
        Some(thumb) => thumb,
        None => return 0,
    };
    if track.height == 0 {
        return 0;
    }
    let clamped_row = row.clamp(track.y, track.y + track.height.saturating_sub(1));
    let row_offset = clamped_row.saturating_sub(track.y) as usize;
    let thumb_center = (thumb.len as usize) / 2;
    let desired_top = row_offset.saturating_sub(thumb_center);
    scrollbar_start_from_thumb_top(metrics, track, desired_top)
}

pub fn scrollbar_start_from_drag_row(
    metrics: impl ScrollbarMetrics,
    track: ScrollTrack,
    row: u16,
    grab_row_offset: u16,
) -> usize {
    if track.height == 0 {
        return 0;
    }
    let clamped_row = row.clamp(track.y, track.y + track.height.saturating_sub(1));
    let row_offset = clamped_row.saturating_sub(track.y) as usize;
    let desired_top = row_offset.saturating_sub(grab_row_offset as usize);
    scrollbar_start_from_thumb_top(metrics, track, desired_top)
}

pub fn scrollbar_offset_from_row(metrics: ScrollMetrics, track: ScrollTrack, row: u16) -> usize {
    if scrollbar_thumb(metrics, track).is_none() {
        return 0;
    }
    metrics
        .max_offset_from_bottom
        .saturating_sub(scrollbar_start_from_row(metrics, track, row))
}

pub fn scrollbar_offset_from_drag_row(
    metrics: ScrollMetrics,
    track: ScrollTrack,
    row: u16,
    grab_row_offset: u16,
) -> usize {
    if track.height == 0 {
        return 0;
    }
    metrics
        .max_offset_from_bottom
        .saturating_sub(scrollbar_start_from_drag_row(
            metrics,
            track,
            row,
            grab_row_offset,
        ))
}

#[cfg(test)]
mod tests {
    use super::{
        ListScroll, ScrollTrack, ScrollbarPart, scrollbar_rows, scrollbar_start_from_drag_row,
        scrollbar_start_from_row, scrollbar_thumb, scrollbar_thumb_grab_offset,
    };

    #[test]
    fn track_height_is_clamped_so_the_far_edge_fits() {
        let track = ScrollTrack::new(65534, 4);
        assert_eq!(track.y(), 65534);
        assert_eq!(track.height(), 1);
        assert_eq!(track.bottom(), u16::MAX);
        assert_eq!(ScrollTrack::new(u16::MAX, 3).height(), 0);
        assert_eq!(ScrollTrack::new(3, 4).height(), 4);
    }

    #[test]
    fn track_at_the_top_of_the_row_range_stays_in_bounds() {
        let metrics = ListScroll::new(10, 10, 5);
        let track = ScrollTrack::new(65534, 4);

        let thumb = scrollbar_thumb(metrics, track).expect("scrollable list shows a thumb");
        assert_eq!((thumb.top, thumb.len), (65534, 1));
        assert_eq!(scrollbar_thumb_grab_offset(metrics, track, 65534), Some(0));
        assert_eq!(scrollbar_thumb_grab_offset(metrics, track, u16::MAX), None);
        assert_eq!(scrollbar_start_from_row(metrics, track, u16::MAX), 0);
        assert_eq!(
            scrollbar_start_from_drag_row(metrics, track, u16::MAX, u16::MAX),
            0
        );
        assert_eq!(
            scrollbar_rows(metrics, track).collect::<Vec<_>>(),
            vec![(65534, ScrollbarPart::Thumb)]
        );

        assert_eq!(
            scrollbar_thumb(metrics, ScrollTrack::new(u16::MAX, 4)),
            None
        );
    }

    #[test]
    fn rows_mark_the_thumb_inside_the_track() {
        let track = ScrollTrack::new(2, 4);
        let parts = |start| {
            scrollbar_rows(ListScroll::new(start, 4, 4), track)
                .map(|(row, part)| (row, part == ScrollbarPart::Thumb))
                .collect::<Vec<_>>()
        };
        assert_eq!(parts(0), vec![(2, true), (3, true), (4, false), (5, false)]);
        assert_eq!(parts(4), vec![(2, false), (3, false), (4, true), (5, true)]);
        assert_eq!(
            scrollbar_rows(ListScroll::new(0, 0, 4), track).count(),
            0,
            "nothing is painted without a thumb"
        );
    }
}
