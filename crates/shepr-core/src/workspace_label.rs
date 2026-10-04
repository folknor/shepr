use std::path::Path;

/// The name a workspace gets when it is given none, or a blank one: the name of
/// the directory it is created (or renamed) in. The home directory is no
/// exception, so it reads as the user's name. The root has no directory name and
/// reads as `/`.
pub fn default_workspace_name(cwd: &Path) -> String {
    cwd.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map_or_else(|| cwd.display().to_string(), str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_name_is_the_directory_name() {
        assert_eq!(
            default_workspace_name(Path::new("/repos/example/nested")),
            "nested"
        );
        assert_eq!(default_workspace_name(Path::new("/home/user")), "user");
        assert_eq!(default_workspace_name(Path::new("/")), "/");
    }
}
