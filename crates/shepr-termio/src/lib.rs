//! Host terminal I/O for the client: framing and parsing the host's input
//! bytes, the fixed and configured key tables the client routes by, copy-mode
//! keys, host terminal modes, title and clipboard writes, theme queries, and
//! blitting frames to the host. Terminal vocabulary and child-facing encoding
//! shared with the emulator live in `shepr-term`.

pub mod blit;
pub mod copy_mode;
pub mod host_term;
pub mod input;
pub mod limits;

pub use input::raw_input;

/// Config for this crate's unit tests. It cannot take the shared
/// `shepr-test-fixtures` crate, which is built on this one, so it validates
/// through the same public seam that crate uses.
#[cfg(test)]
mod test_config {
    use std::path::Path;

    use shepr_config::{ClientConfig, ValidatedClientConfig};
    use shepr_paths::AppPaths;

    /// `source` as a config document, validated as a launch would validate
    /// it, on paths that cannot be written to.
    pub(crate) fn validated(source: &str) -> ValidatedClientConfig {
        let config: ClientConfig = toml::from_str(source).expect("test config parses");
        let root = Path::new("/nonexistent/shepr-test-config");
        ValidatedClientConfig::validate(
            &config,
            Some(source),
            AppPaths::rooted_at(root, Some(root), None).expect("short test root"),
        )
        .expect("test config is valid")
    }
}
