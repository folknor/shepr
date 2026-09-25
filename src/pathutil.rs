use std::ffi::OsString;
use std::path::PathBuf;

pub(crate) fn expand_tilde_path(path: &str) -> PathBuf {
    expand_tilde_path_from_env(path, |key| std::env::var_os(key))
}

fn expand_tilde_path_from_env(path: &str, env: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    let home = || env("HOME").map(PathBuf::from);
    if path == "~" {
        return home().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(path));
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_path_uses_home_when_available() {
        assert_eq!(
            expand_tilde_path_from_env("~/.shepr/state", |key| match key {
                "HOME" => Some("/home/me".into()),
                _ => None,
            }),
            PathBuf::from("/home/me/.shepr/state")
        );
        assert_eq!(
            expand_tilde_path_from_env("/tmp/state", |_| None),
            PathBuf::from("/tmp/state")
        );
    }

    #[test]
    fn tilde_expansion_keeps_backslash_literal() {
        assert_eq!(
            expand_tilde_path_from_env(r"~\.shepr\state", |key| match key {
                "HOME" => Some("/home/me".into()),
                _ => None,
            }),
            PathBuf::from(r"~\.shepr\state")
        );
    }
}
