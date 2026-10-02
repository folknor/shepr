use std::path::Path;

use shepr_protocol::command::WorkspaceCheckoutRootParams;

use crate::app::App;

impl App {
    /// `workspace.checkout_root`: the checkout root Git reports for a directory
    /// on this host, which the client derives a new workspace's default label
    /// from. A directory outside any repository, or one that is not a directory
    /// here, is an ordinary `None`; a failure that kept Git from answering is an
    /// error, and the client falls back to a path-based label. An unusable
    /// `HOME` just means the `~` label is not offered.
    ///
    /// The server loop resolves the request data here. The directory stat and
    /// Git query run in a worker; their result returns to the loop for its
    /// ordered endpoint reply outbox.
    pub(crate) fn prepare_workspace_checkout_root(
        &self,
        params: &WorkspaceCheckoutRootParams,
    ) -> Result<(std::path::PathBuf, Option<String>), shepr_protocol::command::EndpointError> {
        let cwd = super::cwd::launch_cwd(&params.cwd)?;
        let home = self
            .paths
            .home_dir()
            .and_then(|home| home.to_str().map(str::to_owned));
        Ok((cwd, home))
    }

    /// Does the blocking filesystem and Git work for `workspace.checkout_root`.
    pub(crate) fn checkout_root_for_worker(cwd: &Path) -> Result<Option<String>, String> {
        checkout_root(cwd)
    }
}

fn checkout_root(cwd: &Path) -> Result<Option<String>, String> {
    match std::fs::metadata(cwd) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot stat {}: {error}", cwd.display())),
    }
    let output = shepr_mux::git::run_git(cwd, &["rev-parse", "--show-toplevel"])
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
    Ok(Some(root.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_support::{IsolatedEnv, ScratchDir};

    fn app() -> App {
        App::new(
            &shepr_config::ServerConfig::default(),
            crate::app::AppPolicy::Test,
        )
    }

    /// The prepare and worker halves, as the server loop runs them.
    fn checkout_root_of(cwd: &Path) -> Result<Option<String>, String> {
        let (cwd, _home) = app()
            .prepare_workspace_checkout_root(&WorkspaceCheckoutRootParams {
                cwd: cwd.display().to_string(),
            })
            .map_err(|error| error.to_string())?;
        App::checkout_root_for_worker(&cwd)
    }

    #[test]
    fn relative_cwd_is_refused() {
        let _env = IsolatedEnv::new();
        let error = checkout_root_of(Path::new("relative")).expect_err("a relative cwd is refused");
        assert!(error.contains("must be an absolute path"), "{error}");
    }

    #[test]
    fn missing_directory_has_no_root() {
        let _env = IsolatedEnv::new();
        let scratch = ScratchDir::new("checkout-root-missing");
        let root =
            checkout_root_of(&scratch.path().join("absent")).expect("a checkout root answer");
        assert_eq!(root, None);
    }

    #[test]
    fn nested_directory_reports_the_checkout_root() {
        let _env = IsolatedEnv::new();
        let scratch = ScratchDir::new("checkout-root-repo");
        let repo = scratch.path().join("repo");
        let nested = repo.join("a").join("b");
        std::fs::create_dir_all(&nested).expect("test precondition");
        let init = shepr_mux::git::run_git(&repo, &["init", "--quiet"]).expect("test precondition");
        assert!(init.status.success(), "git init failed");

        let root = checkout_root_of(&nested)
            .expect("a checkout root answer")
            .expect("a checkout root");
        assert_eq!(
            std::fs::canonicalize(root).expect("root exists"),
            std::fs::canonicalize(&repo).expect("repo exists")
        );
    }
}
