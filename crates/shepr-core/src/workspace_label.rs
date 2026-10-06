use std::path::Path;

/// The label a workspace gets when no nonblank label is supplied: the trimmed
/// name of the directory it is created (or renamed) in, or the trimmed whole
/// path when that name is blank. The result is always nonempty and unpadded.
/// The home directory is no exception, so it reads as the user's name; the
/// root has no directory name and reads as `/`.
pub fn default_workspace_label(cwd: &Path) -> String {
    let name = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty());
    if let Some(name) = name {
        return name.to_owned();
    }
    let path = cwd.display().to_string();
    let path = path.trim();
    if path.is_empty() {
        "/".to_owned()
    } else {
        path.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_label_is_the_trimmed_directory_name() {
        assert_eq!(
            default_workspace_label(Path::new("/repos/example/nested")),
            "nested"
        );
        assert_eq!(default_workspace_label(Path::new("/home/user")), "user");
        assert_eq!(default_workspace_label(Path::new("/")), "/");
    }
}
