pub mod blit;
pub mod copy_mode;
pub mod host_term;
pub mod input;

pub use input::raw_input;
pub mod selection_render;

/// Config for this crate's unit tests. It cannot take the shared
/// `shepr-test-fixtures` crate, which is built on this one, so it validates
/// through the same public seam that crate uses.
#[cfg(test)]
mod test_config {
    use std::path::Path;

    use shepr_config::{AppPaths, Config, ValidatedConfig};

    /// `source` as a config document, validated as a launch would validate
    /// it, on paths that cannot be written to.
    pub(crate) fn validated(source: &str) -> ValidatedConfig {
        let config: Config = toml::from_str(source).expect("test config parses");
        let root = Path::new("/nonexistent/shepr-test-config");
        ValidatedConfig::from_values(
            config,
            Some(source),
            AppPaths::rooted_at(root, Some(root), None),
        )
        .expect("test config is valid")
    }
}
