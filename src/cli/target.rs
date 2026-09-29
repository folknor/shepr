use std::cell::Cell;
use std::ops::Deref;

use shepr_api::client::ApiClient;

/// The local server a CLI command talks to, plus whether this process has
/// already confirmed that server runs the same build.
pub(super) struct CliContext {
    paths: shepr_config::AppPaths,
    build_checked: Cell<bool>,
}

impl CliContext {
    pub(super) fn local(paths: shepr_config::AppPaths) -> Self {
        Self {
            paths,
            build_checked: Cell::new(false),
        }
    }

    pub(super) fn build_checked(&self) -> bool {
        self.build_checked.get()
    }

    pub(super) fn mark_build_checked(&self) {
        self.build_checked.set(true);
    }
}

impl Deref for CliContext {
    type Target = shepr_config::AppPaths;

    fn deref(&self) -> &Self::Target {
        &self.paths
    }
}

pub(super) fn api_client(context: &CliContext) -> ApiClient {
    ApiClient::local(context)
}

pub(super) fn restart_guidance(context: &CliContext) -> String {
    shepr_api::session::restart_after_update_guidance_for(context)
}

pub(super) fn socket_label(context: &CliContext) -> String {
    shepr_api::socket_path(context).display().to_string()
}

#[cfg(test)]
impl CliContext {
    pub(super) fn test_local(paths: shepr_config::AppPaths) -> Self {
        Self::local(paths)
    }
}
