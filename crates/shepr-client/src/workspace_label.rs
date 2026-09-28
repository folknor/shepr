use std::path::{Path, PathBuf};

pub(super) fn derive_label_from_cwd(cwd: &Path) -> String {
    let repo_root = match git_root_for_workspace_label(cwd) {
        Ok(repo_root) => repo_root,
        Err(error) => {
            tracing::warn!(cwd = %cwd.display(), %error, "could not determine Git workspace label");
            None
        }
    };
    // Only a cwd outside Git can be labelled `~`. An unusable `HOME` just
    // means that label is not offered.
    let home = repo_root
        .is_none()
        .then(|| shepr_core::pathutil::home_dir().ok())
        .flatten();
    shepr_core::workspace_label::workspace_label_from_cwd(
        cwd,
        repo_root.as_deref(),
        home.as_deref(),
    )
}

/// The checkout root Git reports for `cwd`. A cwd outside any repository is an
/// ordinary `None`; a failure that kept Git from answering is an error for the
/// caller to report, and the label falls back as if outside Git.
fn git_root_for_workspace_label(cwd: &Path) -> Result<Option<PathBuf>, String> {
    let output = shepr_platform::git::run_git(cwd, &["rev-parse", "--show-toplevel"])
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        // The runner fixes the locale, so Git's message is stable.
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("not a git repository") {
            return Ok(None);
        }
        return Err(format!(
            "git rev-parse --show-toplevel failed with {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| "git rev-parse --show-toplevel returned non-UTF-8 output".to_owned())?;
    let root = stdout.trim();
    if root.is_empty() {
        return Err("git rev-parse --show-toplevel returned no path".to_owned());
    }
    Ok(Some(PathBuf::from(root)))
}
