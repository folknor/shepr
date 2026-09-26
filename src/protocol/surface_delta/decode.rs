//! Allocation-bounded decoder for the surface-delta codec.
//!
//! The wire codec bounds every length prefix by the remaining input, which is
//! necessary for strings but is not a logical item limit: a frame near the
//! size cap could still announce hundreds of thousands of rows, panes or
//! cells. This decoder therefore reads the delta field by field with the
//! codec's `Decoder` primitives, checking each collection's count and the grid
//! budget before decoding (or reserving space for) the items it announces.
//! Leaf values (cells, strings, rectangles, pane metadata) go through their
//! ordinary serde `Deserialize` implementations, so the byte layout is exactly
//! what `codec::to_vec(&SurfaceDelta { .. })` produces.
//!
//! The price is that `decode_surface`, `decode_frame` and `decode_splits`
//! restate the field order of `PaneSurfaceFrame`, `FrameData` and
//! `PaneSurfaceSplit` from `protocol/wire.rs`. Two guards keep them in step:
//! the struct literals below stop compiling when a field is added or removed,
//! and `bounded_decoder_matches_the_serde_wire_layout` gives every field a
//! distinct value and compares the whole decoded surface, so reordering
//! fields in `wire.rs` (even two of the same type) fails that test. Change
//! those structs and this file together.

use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD};

use super::SurfaceDelta;
use crate::protocol::codec::{CodecError, Decoder};
use crate::protocol::{
    CellData, CursorState, FrameData, MAX_FRAME_SIZE, MAX_SURFACE_CELLS, MAX_SURFACE_DIMENSION,
    PaneSurfaceFrame, PaneSurfacePane, PaneSurfacePatchRow, PaneSurfaceSplit,
    PaneSurfaceSplitDirection, SurfaceRect,
};

const MAX_PANES: usize = 4096;
const MAX_SPLITS: usize = 4096;
const MAX_SPLIT_PATH: usize = 4096;
const MAX_HYPERLINKS: usize = 65_536;

pub(super) fn decode(
    data: &str,
    expected: Option<(u16, u16)>,
) -> Result<SurfaceDelta<PaneSurfaceFrame>, String> {
    // Checking before base64 decoding also bounds that allocation, and with it
    // every length prefix the codec will accept.
    let Some(max_encoded_len) = base64::encoded_len(MAX_FRAME_SIZE, false) else {
        return Err("surface delta frame limit overflow".into());
    };
    if data.len() > max_encoded_len {
        return Err("surface delta exceeds the frame limit".into());
    }
    let bytes = STANDARD_NO_PAD
        .decode(data)
        .map_err(|error| error.to_string())?;
    decode_bytes(&bytes, expected)
}

fn decode_bytes(
    bytes: &[u8],
    expected: Option<(u16, u16)>,
) -> Result<SurfaceDelta<PaneSurfaceFrame>, String> {
    let mut decoder = Decoder::new(bytes);
    let delta = decode_delta(&mut decoder, expected).map_err(|error| error.to_string())?;
    if decoder.finish().is_err() {
        return Err("trailing surface delta bytes".into());
    }
    Ok(delta)
}

fn decode_delta(
    decoder: &mut Decoder<'_>,
    expected: Option<(u16, u16)>,
) -> Result<SurfaceDelta<PaneSurfaceFrame>, CodecError> {
    let base_projection_revision = decoder.decode::<u64>()?;
    let base_surface_revision = decoder.decode::<u64>()?;
    let surface = decode_surface(decoder)?;

    let metadata_dimensions = (surface.frame.width, surface.frame.height);
    let (width, height) = match expected {
        Some(expected) if expected != metadata_dimensions => {
            return Err(CodecError::Invalid(
                "surface dimensions do not match the baseline",
            ));
        }
        Some(expected) => expected,
        None => metadata_dimensions,
    };
    checked_grid_size(width, height)?;
    let rows = decode_rows(decoder, width, height)?;

    Ok(SurfaceDelta {
        base_projection_revision,
        base_surface_revision,
        surface,
        rows,
    })
}

fn decode_surface(decoder: &mut Decoder<'_>) -> Result<PaneSurfaceFrame, CodecError> {
    let boot_id = decoder.decode::<String>()?;
    let projection_revision = decoder.decode::<u64>()?;
    let surface_revision = decoder.decode::<u64>()?;
    let frame = decode_frame(decoder)?;
    let panes = decode_bounded_vec::<PaneSurfacePane>(decoder, MAX_PANES, "too many panes")?;
    let splits = decode_splits(decoder)?;
    Ok(PaneSurfaceFrame {
        boot_id,
        projection_revision,
        surface_revision,
        frame,
        panes,
        splits,
    })
}

fn decode_frame(decoder: &mut Decoder<'_>) -> Result<FrameData, CodecError> {
    // Delta metadata requires an empty cell list, while full surfaces need a
    // bounded cell list. A normal serde decode of FrameData cannot apply
    // those different limits at this field occurrence, and would allocate the
    // announced cells before rejecting them. Removing this context-specific
    // read needs either a separate delta-metadata frame type or a codec seed
    // that carries per-field limits into serde.
    require_empty_sequence(decoder, "surface metadata contains main cells")?;
    let width = decoder.decode::<u16>()?;
    let height = decoder.decode::<u16>()?;
    checked_grid_size(width, height)?;
    let cursor = decoder.decode::<Option<CursorState>>()?;
    let hyperlinks = decode_bounded_vec::<String>(decoder, MAX_HYPERLINKS, "too many hyperlinks")?;
    Ok(FrameData {
        cells: Vec::new(),
        width,
        height,
        cursor,
        hyperlinks,
    })
}

fn decode_splits(decoder: &mut Decoder<'_>) -> Result<Vec<PaneSurfaceSplit>, CodecError> {
    let count = decode_count(decoder, MAX_SPLITS, "too many splits")?;
    let mut splits = Vec::with_capacity(count);
    for _ in 0..count {
        splits.push(PaneSurfaceSplit {
            direction: decoder.decode::<PaneSurfaceSplitDirection>()?,
            pos: decoder.decode::<u16>()?,
            area: decoder.decode::<SurfaceRect>()?,
            hit_rect: decoder.decode::<SurfaceRect>()?,
            path: decode_bounded_vec::<bool>(decoder, MAX_SPLIT_PATH, "split path too long")?,
        });
    }
    Ok(splits)
}

fn decode_rows(
    decoder: &mut Decoder<'_>,
    width: u16,
    height: u16,
) -> Result<Vec<PaneSurfacePatchRow>, CodecError> {
    let cell_budget = checked_grid_size(width, height)?;
    let count = decode_count(
        decoder,
        super::MAX_SPANS.min(cell_budget),
        "too many surface delta spans",
    )?;
    let mut spans = crate::protocol::PatchSpanCheck::new(width, height);
    let mut total_cells = 0usize;
    let mut rows = Vec::with_capacity(count);
    for _ in 0..count {
        let x = decoder.decode::<u16>()?;
        let y = decoder.decode::<u16>()?;
        let cells_len = decoder.read_len()?;
        spans.push(x, y, cells_len).map_err(CodecError::Invalid)?;
        total_cells = total_cells
            .checked_add(cells_len)
            .ok_or(CodecError::Invalid("surface delta cell budget overflow"))?;
        if total_cells > cell_budget {
            return Err(CodecError::Invalid("surface delta cell budget exceeded"));
        }
        let mut cells = Vec::with_capacity(cells_len);
        for _ in 0..cells_len {
            cells.push(decoder.decode::<CellData>()?);
        }
        rows.push(PaneSurfacePatchRow { x, y, cells });
    }
    Ok(rows)
}

fn require_empty_sequence(
    decoder: &mut Decoder<'_>,
    message: &'static str,
) -> Result<(), CodecError> {
    if decoder.read_len()? == 0 {
        Ok(())
    } else {
        Err(CodecError::Invalid(message))
    }
}

fn decode_bounded_vec<'de, T>(
    decoder: &mut Decoder<'de>,
    max: usize,
    message: &'static str,
) -> Result<Vec<T>, CodecError>
where
    T: serde::Deserialize<'de>,
{
    let count = decode_count(decoder, max, message)?;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(decoder.decode::<T>()?);
    }
    Ok(values)
}

fn decode_count(
    decoder: &mut Decoder<'_>,
    max: usize,
    message: &'static str,
) -> Result<usize, CodecError> {
    let count = decoder.read_len()?;
    if count > max {
        Err(CodecError::Invalid(message))
    } else {
        Ok(count)
    }
}

fn checked_grid_size(width: u16, height: u16) -> Result<usize, CodecError> {
    if width > MAX_SURFACE_DIMENSION || height > MAX_SURFACE_DIMENSION {
        return Err(CodecError::Invalid("surface dimensions exceed the limit"));
    }
    let cells = usize::from(width) * usize::from(height);
    if cells > MAX_SURFACE_CELLS {
        Err(CodecError::Invalid("surface cell count exceeds the limit"))
    } else {
        Ok(cells)
    }
}

/// Mirrors all decoder-side metadata limits. The sparse sender must fall back
/// to the full-frame codec when this returns false.
pub(super) fn metadata_fits(surface: &PaneSurfaceFrame) -> bool {
    frame_metadata_fits(&surface.frame)
        && surface.panes.len() <= MAX_PANES
        && surface.splits.len() <= MAX_SPLITS
        && surface
            .splits
            .iter()
            .all(|split| split.path.len() <= MAX_SPLIT_PATH)
}

fn frame_metadata_fits(frame: &FrameData) -> bool {
    checked_grid_size(frame.width, frame.height).is_ok_and(|cells| frame.cells.len() == cells)
        && frame.hyperlinks.len() <= MAX_HYPERLINKS
}

#[cfg(test)]
mod tests {
    use serde::{Serialize, Serializer, ser::SerializeSeq as _};

    use super::*;
    use crate::protocol::codec;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn cell() -> CellData {
        CellData {
            symbol: "x".into(),
            fg: 1,
            bg: 2,
            modifier: 0,
            skip: false,
            hyperlink: None,
        }
    }

    fn surface(width: u16, height: u16) -> PaneSurfaceFrame {
        PaneSurfaceFrame {
            boot_id: "boot".into(),
            projection_revision: 2,
            surface_revision: 3,
            frame: FrameData {
                cells: Vec::new(),
                width,
                height,
                cursor: None,
                hyperlinks: Vec::new(),
            },
            panes: Vec::new(),
            splits: Vec::new(),
        }
    }

    fn encoded<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
        codec::to_vec(value)
    }

    fn rect(seed: u16) -> SurfaceRect {
        SurfaceRect {
            x: seed,
            y: seed + 1,
            width: seed + 2,
            height: seed + 3,
        }
    }

    /// A surface whose every hand-decoded field holds a value distinct from
    /// its neighbours of the same type, so a field-order mismatch between
    /// this decoder and `wire.rs` cannot decode to an equal value.
    fn populated_surface(width: u16, height: u16) -> PaneSurfaceFrame {
        use crate::protocol::PaneSurfaceScrollMetrics;

        let mut surface = surface(width, height);
        surface.boot_id = "boot-layout".into();
        surface.projection_revision = 11;
        surface.surface_revision = 12;
        surface.frame.cursor = Some(CursorState {
            x: 1,
            y: 1,
            visible: true,
            shape: 5,
        });
        surface.frame.hyperlinks = vec!["https://a.example".into(), "https://b.example".into()];
        surface.panes = vec![PaneSurfacePane {
            pane_id: "w1:p1".into(),
            content_revision: 13,
            rect: rect(10),
            inner_rect: rect(20),
            scrollbar_rect: Some(rect(30)),
            scroll: Some(PaneSurfaceScrollMetrics {
                offset_from_bottom: 14,
                max_offset_from_bottom: 15,
                viewport_rows: 16,
            }),
            focused: true,
            mouse_reporting: false,
            sgr_pixel_mouse: true,
            alternate_screen_active: false,
            pixel_width: 17,
            pixel_height: 18,
        }];
        surface.splits = vec![
            PaneSurfaceSplit {
                direction: PaneSurfaceSplitDirection::Vertical,
                pos: 19,
                area: rect(40),
                hit_rect: rect(50),
                path: vec![true, false],
            },
            PaneSurfaceSplit {
                direction: PaneSurfaceSplitDirection::Horizontal,
                pos: 21,
                area: rect(60),
                hit_rect: rect(70),
                path: vec![false],
            },
        ];
        surface
    }

    #[test]
    fn bounded_decoder_matches_the_serde_wire_layout() -> TestResult {
        let surface = populated_surface(4, 2);
        let mut linked = cell();
        linked.hyperlink = Some(1);
        let delta = SurfaceDelta {
            base_projection_revision: 1,
            base_surface_revision: 2,
            surface: surface.clone(),
            rows: vec![
                PaneSurfacePatchRow {
                    x: 1,
                    y: 0,
                    cells: vec![linked.clone()],
                },
                PaneSurfacePatchRow {
                    x: 2,
                    y: 1,
                    cells: vec![cell(), linked],
                },
            ],
        };
        let decoded = decode_bytes(&encoded(&delta)?, Some((4, 2)))?;
        assert_eq!(decoded.base_projection_revision, 1);
        assert_eq!(decoded.base_surface_revision, 2);
        assert_eq!(decoded.surface, surface);
        assert_eq!(decoded.rows, delta.rows);
        Ok(())
    }

    #[test]
    fn rejects_nonempty_metadata_grids_before_cell_allocation() -> TestResult {
        let mut main = surface(1, 1);
        main.frame.cells.push(cell());
        let delta = SurfaceDelta {
            base_projection_revision: 1,
            base_surface_revision: 1,
            surface: main,
            rows: Vec::<PaneSurfacePatchRow>::new(),
        };
        assert!(decode_bytes(&encoded(&delta)?, Some((1, 1))).is_err());
        Ok(())
    }

    struct CountOnly(usize);

    impl Serialize for CountOnly {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.serialize_seq(Some(self.0))?.end()
        }
    }

    /// A forged count followed by enough filler bytes that the codec's
    /// input-length bound does not reject it first, so only the protocol
    /// limit can.
    struct CountWithFiller(usize);

    impl Serialize for CountWithFiller {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeTuple as _;
            let mut tuple = serializer.serialize_tuple(2)?;
            tuple.serialize_element(&CountOnly(self.0))?;
            for _ in 0..self.0 {
                tuple.serialize_element(&0u8)?;
            }
            tuple.end()
        }
    }

    #[test]
    fn rejects_excessive_span_count_without_allocating_the_claimed_rows() -> TestResult {
        let bare = SurfaceDelta {
            base_projection_revision: 1,
            base_surface_revision: 1,
            surface: surface(2, 2),
            rows: CountOnly(super::super::MAX_SPANS + 1),
        };
        assert!(decode_bytes(&encoded(&bare)?, Some((2, 2))).is_err());

        let padded = SurfaceDelta {
            base_projection_revision: 1,
            base_surface_revision: 1,
            surface: surface(2, 2),
            rows: CountWithFiller(super::super::MAX_SPANS + 1),
        };
        let error = decode_bytes(&encoded(&padded)?, Some((2, 2)))
            .err()
            .ok_or("padded span count should be rejected")?;
        assert!(error.contains("too many surface delta spans"), "{error}");
        Ok(())
    }

    #[test]
    fn accepts_max_spans_within_the_grid_cell_budget() -> TestResult {
        let rows = (0..super::super::MAX_SPANS)
            .map(
                |index| -> Result<PaneSurfacePatchRow, std::num::TryFromIntError> {
                    Ok(PaneSurfacePatchRow {
                        x: u16::try_from(index % 64)?,
                        y: u16::try_from(index / 64)?,
                        cells: vec![cell()],
                    })
                },
            )
            .collect::<Result<Vec<_>, _>>()?;
        let delta = SurfaceDelta {
            base_projection_revision: 1,
            base_surface_revision: 1,
            surface: surface(64, 64),
            rows,
        };
        let decoded = decode_bytes(&encoded(&delta)?, Some((64, 64)))?;
        assert_eq!(decoded.rows.len(), super::super::MAX_SPANS);
        Ok(())
    }

    #[test]
    fn row_bounds_use_the_actual_baseline() -> TestResult {
        let delta = SurfaceDelta {
            base_projection_revision: 1,
            base_surface_revision: 1,
            surface: surface(100, 100),
            rows: vec![PaneSurfacePatchRow {
                x: 2,
                y: 0,
                cells: vec![cell()],
            }],
        };
        assert!(decode_bytes(&encoded(&delta)?, Some((2, 2))).is_err());
        Ok(())
    }

    struct SurfaceWithPanes<'a, P> {
        source: &'a PaneSurfaceFrame,
        panes: P,
    }

    impl<P: Serialize> Serialize for SurfaceWithPanes<'_, P> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeTuple as _;
            let mut tuple = serializer.serialize_tuple(6)?;
            tuple.serialize_element(&self.source.boot_id)?;
            tuple.serialize_element(&self.source.projection_revision)?;
            tuple.serialize_element(&self.source.surface_revision)?;
            tuple.serialize_element(&self.source.frame)?;
            tuple.serialize_element(&self.panes)?;
            tuple.serialize_element(&self.source.splits)?;
            tuple.end()
        }
    }

    #[test]
    fn rejects_excessive_metadata_count_without_allocating_the_claimed_items() -> TestResult {
        let source = surface(2, 2);
        let bare = SurfaceDelta {
            base_projection_revision: 1,
            base_surface_revision: 1,
            surface: SurfaceWithPanes {
                source: &source,
                panes: CountOnly(MAX_PANES + 1),
            },
            rows: Vec::<PaneSurfacePatchRow>::new(),
        };
        assert!(decode_bytes(&encoded(&bare)?, Some((2, 2))).is_err());

        let padded = SurfaceDelta {
            base_projection_revision: 1,
            base_surface_revision: 1,
            surface: SurfaceWithPanes {
                source: &source,
                panes: CountWithFiller(MAX_PANES + 1),
            },
            rows: Vec::<PaneSurfacePatchRow>::new(),
        };
        let error = decode_bytes(&encoded(&padded)?, Some((2, 2)))
            .err()
            .ok_or("padded pane count should be rejected")?;
        assert!(error.contains("too many panes"), "{error}");
        Ok(())
    }

    #[test]
    fn rejects_trailing_bytes() -> TestResult {
        let delta = SurfaceDelta {
            base_projection_revision: 1,
            base_surface_revision: 1,
            surface: surface(2, 2),
            rows: Vec::<PaneSurfacePatchRow>::new(),
        };
        let mut bytes = encoded(&delta)?;
        assert!(decode_bytes(&bytes, Some((2, 2))).is_ok());
        bytes.push(0);
        assert_eq!(
            decode_bytes(&bytes, Some((2, 2))).err().as_deref(),
            Some("trailing surface delta bytes")
        );
        Ok(())
    }

    #[test]
    fn sender_eligibility_matches_grid_and_metadata_limits() {
        let mut exact = surface(1024, 128);
        exact.frame.cells = vec![cell(); MAX_SURFACE_CELLS];
        assert!(metadata_fits(&exact));
        exact.frame.cells.pop();
        assert!(!metadata_fits(&exact));

        let mut candidate = surface(1024, 129);
        assert!(!metadata_fits(&candidate));
        candidate.frame.width = MAX_SURFACE_DIMENSION + 1;
        candidate.frame.height = 1;
        candidate.frame.cells = vec![cell()];
        assert!(!metadata_fits(&candidate));
        candidate.frame.width = 1;
        candidate.frame.hyperlinks = vec![String::new(); MAX_HYPERLINKS + 1];
        assert!(!metadata_fits(&candidate));
    }

    #[test]
    fn base64_input_limit_uses_encoded_characters_for_a_frame_sized_payload() {
        let between_raw_and_encoded_limit = "!".repeat(MAX_FRAME_SIZE + 1);
        let error = decode(&between_raw_and_encoded_limit, None)
            .err()
            .expect("the decoder should reach base64 validation before the character limit");
        assert!(!error.contains("exceeds the frame limit"), "{error}");

        let max_encoded_len = base64::encoded_len(MAX_FRAME_SIZE, false)
            .expect("the frame size has a representable base64 length");
        let over_encoded_limit = "!".repeat(max_encoded_len + 1);
        let error = decode(&over_encoded_limit, None)
            .err()
            .expect("input beyond the decoded frame budget is rejected");
        assert!(error.contains("exceeds the frame limit"), "{error}");
    }
}
