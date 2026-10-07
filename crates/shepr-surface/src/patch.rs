//! Shared changed-span limits and collection for surface patch producers.

use shepr_protocol::PaneSurfacePatchRow;

/// Maximum number of changed spans carried by one surface patch: the wire's
/// own bound on `SurfaceUpdate::spans`, so a collected patch always encodes.
pub const MAX_PATCH_SPANS: usize = shepr_protocol::MAX_SURFACE_PATCH_SPANS;

/// A patch producer tried to add a span after reaching [`MAX_PATCH_SPANS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatchSpanLimitExceeded;

/// Collects patch spans while enforcing the limit before storing each one.
#[derive(Debug, Clone)]
pub struct PatchSpanCollector<T> {
    spans: Vec<T>,
}

impl<T> Default for PatchSpanCollector<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> PatchSpanCollector<T> {
    pub fn new() -> Self {
        Self { spans: Vec::new() }
    }

    pub fn push(&mut self, span: T) -> Result<(), PatchSpanLimitExceeded> {
        if self.spans.len() == MAX_PATCH_SPANS {
            return Err(PatchSpanLimitExceeded);
        }
        self.spans.push(span);
        Ok(())
    }

    pub fn as_slice(&self) -> &[T] {
        &self.spans
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.spans
    }

    pub fn into_vec(self) -> Vec<T> {
        self.spans
    }
}

/// Errors from checking one set of changed spans against its surface grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchSpanError {
    LimitExceeded,
    InvalidRows(&'static str),
}

/// Applies the surface span cap and the wire row-order and geometry rule.
pub fn validate_spans(
    width: u16,
    height: u16,
    spans: &[PaneSurfacePatchRow],
) -> Result<(), PatchSpanError> {
    if spans.len() > MAX_PATCH_SPANS {
        return Err(PatchSpanError::LimitExceeded);
    }
    shepr_protocol::validate_patch_rows(width, height, spans).map_err(PatchSpanError::InvalidRows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collector_accepts_the_cap_and_refuses_one_more_without_growing() {
        let mut spans = PatchSpanCollector::new();
        for span in 0..MAX_PATCH_SPANS {
            spans.push(span).expect("span is within the cap");
        }

        assert_eq!(spans.as_slice().len(), MAX_PATCH_SPANS);
        assert_eq!(spans.push(MAX_PATCH_SPANS), Err(PatchSpanLimitExceeded));
        assert_eq!(spans.as_slice().len(), MAX_PATCH_SPANS);
    }
}
