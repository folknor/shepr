/// Maximum byte length of an agent session ID accepted from hook reports or
/// saved state. This leaves ample room for opaque IDs while bounding persisted
/// and command-line data.
pub(crate) const MAX_SESSION_ID_LEN: usize = 512;

/// Maximum byte length of an agent session path accepted from hook reports or
/// saved state. The ceiling matches the scale of Linux pathname limits
/// and bounds persisted path data.
pub(crate) const MAX_SESSION_PATH_LEN: usize = 4096;
