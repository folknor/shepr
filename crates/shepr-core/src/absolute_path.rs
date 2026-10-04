//! A path known to be absolute, checked once where it enters the program.

use std::ffi::OsStr;
use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};

/// A filesystem path that starts at the root. It is a lexical fact only: the
/// directory may not exist, may be on a hung mount or may vanish later, and the
/// child's chdir stays the judge of that. What the type removes is a relative
/// path, which would resolve against the server's own working directory.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AbsolutePath(PathBuf);

/// The path handed to [`AbsolutePath::new`] was not absolute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotAbsolute(PathBuf);

impl NotAbsolute {
    /// The refused path.
    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl fmt::Display for NotAbsolute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "path {} is not absolute", self.0.display())
    }
}

impl std::error::Error for NotAbsolute {}

impl AbsolutePath {
    /// Admits `path` when it is absolute.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, NotAbsolute> {
        let path = path.into();
        if path.is_absolute() {
            Ok(Self(path))
        } else {
            Err(NotAbsolute(path))
        }
    }

    /// The filesystem root.
    pub fn root() -> Self {
        Self(PathBuf::from("/"))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// `path` taken from this directory, as `Path::join` takes it: an
    /// absolute `path` is itself, a relative one is joined below this one.
    /// Either way the result starts at the root, so no check is repeated.
    pub fn resolve(&self, path: impl AsRef<Path>) -> Self {
        Self(self.0.join(path))
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

/// An absolute path is a path: borrowing it as `Path` widens it and loses
/// nothing. No `Path` method yields an `AbsolutePath`, so the deref cannot mint
/// one from an unchecked path; it only spares `.as_path()` at sites that
/// display or pass the path on. Unlike a numeric identity, a path has no second
/// value space it could be confused with.
impl Deref for AbsolutePath {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for AbsolutePath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<OsStr> for AbsolutePath {
    fn as_ref(&self) -> &OsStr {
        self.0.as_os_str()
    }
}

impl From<AbsolutePath> for PathBuf {
    fn from(path: AbsolutePath) -> Self {
        path.0
    }
}

/// Comparisons with the plain path types are path equality, the same
/// component-wise comparison the derived `AbsolutePath` equality and hash use.
/// They cannot give a false match: a relative path on the other side is simply
/// unequal, since this type never resolves one against a working directory.
/// They exist for comparing against runtime observations and test literals.
macro_rules! path_equality {
    ($($other:ty),*) => {$(
        impl PartialEq<$other> for AbsolutePath {
            fn eq(&self, other: &$other) -> bool {
                self.0.as_path() == AsRef::<Path>::as_ref(other)
            }
        }

        impl PartialEq<AbsolutePath> for $other {
            fn eq(&self, other: &AbsolutePath) -> bool {
                AsRef::<Path>::as_ref(self) == other.0.as_path()
            }
        }
    )*};
}

path_equality!(Path, PathBuf, &Path);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_absolute_paths_are_admitted() {
        assert!(AbsolutePath::new("/").is_ok());
        assert!(AbsolutePath::new("/missing/directory").is_ok());
        for relative in ["", "relative", "./here", "~/home"] {
            let refused = AbsolutePath::new(relative).expect_err("relative path");
            assert_eq!(refused.path(), Path::new(relative));
        }
    }

    #[test]
    fn resolve_joins_a_relative_path_and_keeps_an_absolute_one() {
        let base = AbsolutePath::new("/base").expect("absolute");
        assert_eq!(base.resolve("child/dir"), Path::new("/base/child/dir"));
        assert_eq!(base.resolve("/elsewhere"), Path::new("/elsewhere"));
        assert_eq!(base.resolve(""), Path::new("/base/"));
    }

    #[test]
    fn root_is_absolute_and_reads_back_as_a_path() {
        let root = AbsolutePath::root();
        assert_eq!(root.as_path(), Path::new("/"));
        assert_eq!(PathBuf::from(root), PathBuf::from("/"));
    }
}
