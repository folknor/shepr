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
pub(crate) const MIN_ENCODED_CELL_BYTES: usize = 5;
