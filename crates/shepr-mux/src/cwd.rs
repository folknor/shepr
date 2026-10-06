use std::path::PathBuf;

use shepr_core::absolute_path::AbsolutePath;

/// An `AbsolutePath` that was also seen to be an existing directory when it was
/// observed. The absolute path alone is only lexical (it is what a pane's
/// stored cwd and a saved cwd hold, existing or not); this adds the
/// observation, which can be stale a moment later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsableCwd(AbsolutePath);

impl UsableCwd {
    /// `None` for a relative path, a non-directory, an absent path, or one
    /// that cannot be stat'd: none of them is usable. The last is logged at
    /// debug level with its error, so an unreadable directory is not mistaken
    /// for a missing one when a pane's cwd is not picked up.
    pub fn new(path: PathBuf) -> Option<Self> {
        let path = AbsolutePath::new(path).ok()?;
        match std::fs::metadata(&path) {
            Ok(metadata) => metadata.is_dir().then_some(Self(path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                tracing::debug!(path = %path.display(), %error, "cwd cannot be stat'd");
                None
            }
        }
    }

    /// A directory a pane child has just entered with chdir, which is the
    /// observation this type records; checking it again here would stat it on
    /// the event loop.
    pub(crate) fn entered(path: AbsolutePath) -> Self {
        Self(path)
    }

    pub fn as_path(&self) -> &std::path::Path {
        self.0.as_path()
    }

    pub fn as_absolute(&self) -> &AbsolutePath {
        &self.0
    }

    pub fn into_absolute(self) -> AbsolutePath {
        self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0.into_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::UsableCwd;

    #[test]
    fn cwd_requires_an_absolute_existing_directory() {
        let scratch = crate::test_support::ScratchDir::new("usable-cwd");
        let directory = scratch.to_path_buf();

        assert_eq!(
            UsableCwd::new(directory.clone()).map(UsableCwd::into_path_buf),
            Some(directory.clone())
        );
        assert!(UsableCwd::new(std::path::PathBuf::from("relative")).is_none());
        assert!(UsableCwd::new(directory.join("missing")).is_none());
    }
}
