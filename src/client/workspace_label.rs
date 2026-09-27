use std::path::Path;

pub(super) fn derive_label_from_cwd(cwd: &Path) -> String {
    let repo_root = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|output| output.trim().to_owned())
        .filter(|path| !path.is_empty());
    shepr_platform::workspace_label_from_cwd(cwd, repo_root.as_deref().map(Path::new))
}
