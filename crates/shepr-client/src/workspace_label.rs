use std::path::Path;

pub(super) fn derive_label_from_cwd(cwd: &Path) -> String {
    // host-program-ok: production asks Git for the checkout root
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
    // Only a cwd outside Git can be labelled `~`. An unusable `HOME` just
    // means that label is not offered.
    let home = repo_root
        .is_none()
        .then(|| shepr_core::pathutil::home_dir().ok())
        .flatten();
    shepr_core::workspace_label::workspace_label_from_cwd(
        cwd,
        repo_root.as_deref().map(Path::new),
        home.as_deref(),
    )
}
