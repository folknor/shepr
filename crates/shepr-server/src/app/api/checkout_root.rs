use std::path::Path;

use shepr_protocol::command::WorkspaceCheckoutRootParams;

use crate::app::App;

impl App {
    /// `workspace.checkout_root`: the checkout root shared discovery finds for
    /// a directory on this host, which the client uses for the new-workspace
    /// name suggestion. An admitted workspace's automatic identity is decided
    /// separately by Workspace and its background Git status result. A directory
    /// outside any repository, or one that is not a directory here, is an
    /// ordinary `None`; a filesystem failure that
    /// kept discovery from answering is an error, and the client falls back to
    /// a path-based label. Discovery is the walk the sidebar labels the
    /// created workspace by, so the two agree (a bare repository answers with
    /// its own directory). An unusable `HOME` just means the `~` label is not
    /// offered.
    ///
    /// The server loop resolves the request data here. The directory stat and
    /// discovery walk run in a worker; their result returns to the loop for
    /// its ordered endpoint reply outbox.
    pub(crate) fn prepare_workspace_checkout_root(
        &self,
        params: &WorkspaceCheckoutRootParams,
    ) -> Result<
        (std::path::PathBuf, Option<shepr_protocol::RemotePath>),
        shepr_protocol::command::EndpointError,
    > {
        let cwd = super::cwd::launch_cwd(&params.cwd)?;
        let home = self.paths.home_dir().map(shepr_protocol::RemotePath::from);
        Ok((cwd, home))
    }

    /// Does the blocking discovery work for `workspace.checkout_root`.
    pub(crate) fn checkout_root_for_worker(
        cwd: &Path,
    ) -> Result<Option<shepr_protocol::RemotePath>, shepr_git::GitReadError> {
        checkout_root(cwd)
    }
}

fn checkout_root(
    cwd: &Path,
) -> Result<Option<shepr_protocol::RemotePath>, shepr_git::GitReadError> {
    match std::fs::metadata(cwd) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Ok(None),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(error) => {
            return Err(shepr_git::GitReadError::FileRead {
                path: cwd.to_path_buf(),
                reason: shepr_git::FileReadReason::from(&error),
            });
        }
    }
    // Admission and sidebar refresh use one discovery policy for this answer.
    let Some(root) = shepr_git::discover_checkout_root(cwd)? else {
        return Ok(None);
    };
    Ok(Some(root.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_test_support::{IsolatedEnv, ScratchDir};

    fn app() -> App {
        App::new(
            &shepr_config::ServerConfig::default(),
            crate::app::AppPolicy::Suspended,
        )
    }

    /// The prepare and worker halves, as the server loop runs them.
    fn checkout_root_of(cwd: &Path) -> Result<Option<shepr_protocol::RemotePath>, String> {
        let (cwd, _home) = app()
            .prepare_workspace_checkout_root(&WorkspaceCheckoutRootParams { cwd: cwd.into() })
            .map_err(|error| error.to_string())?;
        App::checkout_root_for_worker(&cwd).map_err(|error| error.to_string())
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
        let init = shepr_git::run_git(&repo, &["init", "--quiet"]).expect("test precondition");
        assert!(init.status.success(), "git init failed");

        let root = checkout_root_of(&nested)
            .expect("a checkout root answer")
            .expect("a checkout root");
        assert_eq!(
            std::fs::canonicalize(root.as_path()).expect("root exists"),
            std::fs::canonicalize(&repo).expect("repo exists")
        );
    }
}
