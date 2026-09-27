//! Build identity helpers.
//!
//! `build.rs` fingerprints the source tree this binary was built from; see
//! its module doc for why a hand-maintained version cannot stand in for it.

include!(concat!(env!("OUT_DIR"), "/build_id.rs"));

/// Package version plus the source fingerprint as semver build metadata, for
/// example `0.1.0+0123456789abcdef`. The package version alone is the same
/// for every build, so it could not tell a stale server or a hand-copied
/// remote binary apart from the current one.
pub fn version() -> String {
    format!("{}+{BUILD_ID}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_carries_the_build_fingerprint() {
        assert_eq!(
            version(),
            format!("{}+{}", env!("CARGO_PKG_VERSION"), BUILD_ID)
        );
        assert_eq!(BUILD_ID.len(), 16);
        assert!(BUILD_ID.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
