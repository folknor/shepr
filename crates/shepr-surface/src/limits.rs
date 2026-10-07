//! Bounds for surface processing.

/// Smallest divisor used to translate row-major buffer positions to cells.
///
/// Empty or malformed zero-width buffers still need a nonzero row length for
/// position arithmetic, so this is the safe floor.
pub(crate) const MIN_BUFFER_ROW_LEN: usize = 1;

/// Fewest bytes one encoded cell can take: a string length prefix, a
/// grid-width discriminant, two color discriminants and a
/// hyperlink option tag, ignoring its symbol and style. The delta planner
/// sizes a full surface from it without encoding the full surface.
///
/// This is a valid but conservative lower bound: a cell's style always adds
/// two more bytes, so the true minimum is larger and the estimate runs low.
/// The planner may then send a full surface for a delta that would have been
/// smaller, which costs bandwidth and never correctness.
pub(crate) const MIN_ENCODED_CELL_BYTES: usize = 5;
