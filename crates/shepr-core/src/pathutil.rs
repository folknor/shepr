use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::env::{self, EnvError, EnvVar};

/// `$HOME`, read under the environment policy (`crate::env`). An unset or
/// empty `HOME` is an error rather than a fallback, and a relative, padded or
/// non-UTF-8 one is refused: every caller builds a path under it, and an
/// invalid home would make that path relative to the current directory.
pub fn home_dir() -> io::Result<PathBuf> {
    home_dir_from_value(env::read_path(EnvVar::Home))
}

/// Expands a leading bare `~` or `~/` to `$HOME`. The `~user` form is left
/// untouched: resolving another user's home needs a passwd lookup, and
/// silently turning `~bob/x` into `$HOME/bob/x` would point at the wrong place.
/// The suffix is handled as path bytes, so non-UTF-8 names after `~/` expand too.
///
/// A path that needs `$HOME` fails when `HOME` is unset, empty, or refused
/// instead of being returned literally. The literal `~/x` is a relative path,
/// so every caller would go on to resolve it against its working directory: an
/// integration install would create a directory named `~` there, and
/// `--cwd ~/x` would name `$PWD/~/x`. Callers that have a sensible default
/// (the new-terminal cwd policy) apply it on the error themselves.
pub fn expand_tilde_path(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    expand_tilde_path_from(path.as_ref(), &home_dir)
}

/// Expands a leading bare `~` or `~/` using a home directory already resolved
/// by the caller. Paths that do not need HOME pass through unchanged.
pub fn expand_tilde_path_with_home(
    path: impl AsRef<Path>,
    home: Option<&Path>,
) -> io::Result<PathBuf> {
    let path = path.as_ref();
    match tilde_expansion(path.as_os_str()) {
        Some(TildeExpansion::Home) => home.map(Path::to_path_buf).ok_or_else(missing_home_error),
        Some(TildeExpansion::Relative(rest)) => home
            .map(|home| home.join(rest))
            .ok_or_else(missing_home_error),
        None => Ok(path.to_path_buf()),
    }
}

pub fn missing_home_error() -> io::Error {
    io::Error::other("HOME must be set to a non-empty absolute path to locate home directory")
}

fn home_dir_from_value(value: Result<Option<PathBuf>, EnvError>) -> io::Result<PathBuf> {
    value?.ok_or_else(missing_home_error)
}

fn expand_tilde_path_from(
    path: &Path,
    home: &dyn Fn() -> io::Result<PathBuf>,
) -> io::Result<PathBuf> {
    match tilde_expansion(path.as_os_str()) {
        Some(TildeExpansion::Home) => home(),
        Some(TildeExpansion::Relative(rest)) => Ok(home()?.join(rest)),
        None => Ok(path.to_path_buf()),
    }
}

enum TildeExpansion<'a> {
    Home,
    Relative(&'a OsStr),
}

fn tilde_expansion(path: &OsStr) -> Option<TildeExpansion<'_>> {
    let bytes = path.as_bytes();
    if bytes == b"~" {
        Some(TildeExpansion::Home)
    } else if bytes.starts_with(b"~/") {
        let mut suffix = &bytes[2..];
        while suffix.first() == Some(&b'/') {
            suffix = &suffix[1..];
        }
        Some(TildeExpansion::Relative(OsStr::from_bytes(suffix)))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `HOME` as the environment would hand it in, read under the policy.
    fn home_from(raw: Option<&str>) -> io::Result<PathBuf> {
        home_dir_from_value(env::resolve_path(EnvVar::Home, raw.map(OsStr::new)))
    }

    fn expand(path: &str, raw_home: Option<&str>) -> io::Result<PathBuf> {
        expand_tilde_path_from(Path::new(path), &|| home_from(raw_home))
    }

    const HOME: Option<&str> = Some("/home/me");

    #[test]
    fn expand_tilde_path_uses_home_when_available() {
        assert_eq!(
            expand("~/.shepr/state", HOME).expect("test precondition"),
            PathBuf::from("/home/me/.shepr/state")
        );
        assert_eq!(
            expand("~", HOME).expect("test precondition"),
            PathBuf::from("/home/me")
        );
        assert_eq!(
            expand("~//x", HOME).expect("test precondition"),
            PathBuf::from("/home/me/x")
        );
        assert_eq!(
            expand("~///x", HOME).expect("test precondition"),
            PathBuf::from("/home/me/x")
        );
        assert_eq!(
            expand("/tmp/state", None).expect("test precondition"),
            PathBuf::from("/tmp/state")
        );
    }

    #[test]
    fn expand_tilde_path_expands_only_bare_tilde_and_tilde_slash() {
        assert_eq!(
            expand("~bob/x", HOME).expect("test precondition"),
            PathBuf::from("~bob/x")
        );
        assert_eq!(
            expand("/abs/~/x", HOME).expect("test precondition"),
            PathBuf::from("/abs/~/x")
        );
        assert_eq!(
            expand("relative/x", HOME).expect("test precondition"),
            PathBuf::from("relative/x")
        );
    }

    #[test]
    fn home_dir_rejects_missing_empty_relative_and_padded_home() {
        assert!(home_from(None).is_err());
        assert!(home_from(Some("")).is_err());
        assert!(home_from(Some("relative/home")).is_err());
        assert!(home_from(Some("/home/me ")).is_err());
        assert_eq!(
            home_from(HOME).expect("an absolute HOME resolves"),
            PathBuf::from("/home/me")
        );
    }

    #[test]
    fn tilde_expansion_keeps_backslash_literal() {
        assert_eq!(
            expand(r"~\.shepr\state", HOME).expect("test precondition"),
            PathBuf::from(r"~\.shepr\state")
        );
    }

    #[test]
    fn tilde_expansion_keeps_non_utf8_suffix_bytes() {
        let path = Path::new(OsStr::from_bytes(b"~/caf\xe9/state"));
        assert_eq!(
            expand_tilde_path_from(path, &|| home_from(HOME)).expect("test precondition"),
            Path::new(OsStr::from_bytes(b"/home/me/caf\xe9/state"))
        );
        assert_eq!(
            expand_tilde_path_with_home(path, Some(Path::new("/h"))).expect("test precondition"),
            Path::new(OsStr::from_bytes(b"/h/caf\xe9/state"))
        );
    }

    #[test]
    fn tilde_paths_fail_without_a_home() {
        for raw_home in [None, Some("")] {
            assert!(expand("~", raw_home).is_err());
            assert!(expand("~/x", raw_home).is_err());
            // Paths that do not need the home directory still pass through.
            assert_eq!(
                expand("/abs", raw_home).expect("test precondition"),
                PathBuf::from("/abs")
            );
            assert_eq!(
                expand("~bob/x", raw_home).expect("test precondition"),
                PathBuf::from("~bob/x")
            );
            assert!(home_from(raw_home).is_err());
        }
    }
}
