use std::cell::Cell;
use std::ops::Deref;

use shepr_api::client::ApiClient;
use shepr_remote::machine::SavedSshEndpoint;

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

pub(super) fn resolve_machine<'a>(
    profiles: &'a [SavedSshEndpoint],
    selector: &str,
) -> Result<&'a SavedSshEndpoint, String> {
    let profile = if let Some(profile) = profiles
        .iter()
        .find(|profile| profile.id.as_str() == selector)
    {
        profile
    } else {
        let mut matches = profiles.iter().filter(|profile| profile.label == selector);
        let profile = matches
            .next()
            .ok_or_else(|| format!("unknown machine '{selector}'; use `shepr machine list`"))?;
        if matches.next().is_some() {
            return Err(format!(
                "machine label '{selector}' is ambiguous; use its profile ID"
            ));
        }
        profile
    };
    Ok(profile)
}

#[cfg(test)]
impl CliContext {
    pub(super) fn test_local(paths: shepr_config::AppPaths) -> Self {
        Self::local(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_resolution_requires_a_unique_saved_machine() {
        let mac = SavedSshEndpoint::new("mac", "mac-ssh", "agents").expect("test precondition");
        let other =
            SavedSshEndpoint::new("build", "builder", "default").expect("test precondition");
        let profiles = vec![mac.clone(), other];
        assert_eq!(
            resolve_machine(&profiles, "mac").expect("test precondition"),
            &mac
        );
        assert_eq!(
            resolve_machine(&profiles, mac.id.as_str()).expect("test precondition"),
            &mac
        );
        let shadow =
            SavedSshEndpoint::new(mac.id.as_str(), "shadow", "default").expect("test precondition");
        assert_eq!(
            resolve_machine(&[mac.clone(), shadow], mac.id.as_str()).expect("test precondition"),
            &mac
        );
        assert!(resolve_machine(&profiles, "mac-ssh").is_err());
        assert!(resolve_machine(&profiles, "missing").is_err());
        let duplicate =
            SavedSshEndpoint::new("mac", "other", "default").expect("test precondition");
        assert!(resolve_machine(&[mac.clone(), duplicate], "mac").is_err());
    }
}
