use std::path::PathBuf;

/// A directory path that is absolute and usable when it is observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UsableCwd(PathBuf);

impl UsableCwd {
    pub(super) fn new(path: PathBuf) -> Option<Self> {
        (path.is_absolute() && path.is_dir()).then_some(Self(path))
    }

    pub(super) fn into_path_buf(self) -> PathBuf {
        self.0
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
