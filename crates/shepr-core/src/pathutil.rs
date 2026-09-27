use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// `$HOME`. An unset, empty, or relative `HOME` is an error rather than a
/// fallback: every caller builds a path under it, and an invalid home would
/// make that path relative to the current directory.
pub fn home_dir() -> io::Result<PathBuf> {
    home_dir_from_env(&|key| std::env::var_os(key))
}

/// Expands a leading bare `~` or `~/` to `$HOME`. The `~user` form is left
/// untouched: resolving another user's home needs a passwd lookup, and
/// silently turning `~bob/x` into `$HOME/bob/x` would point at the wrong place.
///
/// A path that needs `$HOME` fails when `HOME` is unset, empty, or relative
/// instead of being returned literally. The literal `~/x` is a relative path,
/// so every caller would go on to resolve it against its working directory: an
/// integration install would create a directory named `~` there, and
/// `--cwd ~/x` would name `$PWD/~/x`. Callers that have a sensible default
/// (the new-terminal cwd policy) apply it on the error themselves.
pub fn expand_tilde_path(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    expand_tilde_path_from_env(path.as_ref(), &|key| std::env::var_os(key))
}

/// Expands a leading bare `~` or `~/` using a home directory already resolved
/// by the caller. Paths that do not need HOME pass through unchanged.
pub fn expand_tilde_path_with_home(
    path: impl AsRef<Path>,
    home: Option<&Path>,
) -> io::Result<PathBuf> {
    let path = path.as_ref();
    let Some(raw) = path.to_str() else {
        return Ok(path.to_path_buf());
    };
    if raw == "~" {
        return home.map(Path::to_path_buf).ok_or_else(missing_home_error);
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return home
            .map(|home| home.join(rest))
            .ok_or_else(missing_home_error);
    }
    Ok(path.to_path_buf())
}

pub fn missing_home_error() -> io::Error {
    io::Error::other("HOME must be set to a non-empty absolute path to locate home directory")
}

fn home_dir_from_env(env: &dyn Fn(&str) -> Option<OsString>) -> io::Result<PathBuf> {
    home_dir_from_value(env("HOME"))
}

fn home_dir_from_value(value: Option<OsString>) -> io::Result<PathBuf> {
    value
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(missing_home_error)
}

fn expand_tilde_path_from_env(
    path: &Path,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> io::Result<PathBuf> {
    let Some(raw) = path.to_str() else {
        return Ok(path.to_path_buf());
    };
    if raw == "~" {
        return home_dir_from_env(env);
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return Ok(home_dir_from_env(env)?.join(rest));
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(key: &str) -> Option<OsString> {
        (key == "HOME").then(|| "/home/me".into())
    }

    fn expand(path: &str, env: &dyn Fn(&str) -> Option<OsString>) -> io::Result<PathBuf> {
        expand_tilde_path_from_env(Path::new(path), env)
    }

    #[test]
    fn expand_tilde_path_uses_home_when_available() {
        assert_eq!(
            expand("~/.shepr/state", &home).expect("test precondition"),
            PathBuf::from("/home/me/.shepr/state")
        );
        assert_eq!(
            expand("~", &home).expect("test precondition"),
            PathBuf::from("/home/me")
        );
        assert_eq!(
            expand("/tmp/state", &|_| None).expect("test precondition"),
            PathBuf::from("/tmp/state")
        );
    }

    #[test]
    fn expand_tilde_path_expands_only_bare_tilde_and_tilde_slash() {
        assert_eq!(
            expand("~bob/x", &home).expect("test precondition"),
            PathBuf::from("~bob/x")
        );
        assert_eq!(
            expand("/abs/~/x", &home).expect("test precondition"),
            PathBuf::from("/abs/~/x")
        );
        assert_eq!(
            expand("relative/x", &home).expect("test precondition"),
            PathBuf::from("relative/x")
        );
    }

    #[test]
    fn home_dir_rejects_missing_empty_and_relative_home() {
        assert!(home_dir_from_value(None).is_err());
        assert!(home_dir_from_value(Some(OsString::new())).is_err());
        assert!(home_dir_from_value(Some(OsString::from("relative/home"))).is_err());
    }

    #[test]
    fn tilde_expansion_keeps_backslash_literal() {
        assert_eq!(
            expand(r"~\.shepr\state", &home).expect("test precondition"),
            PathBuf::from(r"~\.shepr\state")
        );
    }

    #[test]
    fn tilde_paths_fail_without_a_home() {
        let unset = |_: &str| None;
        let empty = |_: &str| Some(OsString::new());
        for env in [&unset as &dyn Fn(&str) -> Option<OsString>, &empty] {
            assert!(expand("~", env).is_err());
            assert!(expand("~/x", env).is_err());
            // Paths that do not need the home directory still pass through.
            assert_eq!(
                expand("/abs", env).expect("test precondition"),
                PathBuf::from("/abs")
            );
            assert_eq!(
                expand("~bob/x", env).expect("test precondition"),
                PathBuf::from("~bob/x")
            );
            assert!(home_dir_from_env(env).is_err());
        }
    }
}
