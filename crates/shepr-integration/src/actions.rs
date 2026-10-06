use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;

use shepr_agent::IntegrationTarget;

use super::env::AgentIntegrationPaths;
use super::registry::{agent_present, integration_status, managed_assets};
use super::targets::install;
use super::types::{InstallError, InstallOutcome, IntegrationStatusKind};

/// Installs or updates shepr's hooks for every supported agent present on
/// this host. The server caller decides whether this release-only operation
/// should run because agent configs are shared across build profiles.
///
/// An agent is present when its own config directory already exists; an
/// absent agent is skipped and its directory is never created. A target
/// whose installed integration is already current is left untouched, so a
/// launch that finds nothing to do writes nothing. Everything is reported
/// through `tracing`: a target that cannot be checked or installed is logged
/// and the others still run, and nothing here fails the caller.
///
/// This does file IO, so a server calls it off its startup path.
pub fn install_present_integrations(paths: &AgentIntegrationPaths) {
    let mut python3_available = None;
    for target in IntegrationTarget::all() {
        let label = target.label();
        match install_if_present(paths, target, &mut python3_available) {
            Ok(Some(output)) => {
                for artifact in output.artifacts {
                    crate::logging::artifact_installed(label, &artifact);
                }
                for notice in output.notices {
                    shepr_platform::structured_log!(
                        INFO, event = integration.notice, outcome = "reported",
                        integration = label,
                        notice = %notice,
                        "integration installation notice"
                    );
                }
            }
            Ok(None) => {}
            Err(error) => {
                shepr_platform::structured_log!(
                    WARN, event = integration.install, outcome = "error",
                    integration = label,
                    error_kind = ?error.kind(),
                    %error,
                    "could not check or install the agent integration"
                );
            }
        }
    }
}

/// The install output when `target`'s agent is present and its integration
/// was missing or outdated, `None` when there was nothing to do.
fn install_if_present(
    paths: &AgentIntegrationPaths,
    target: IntegrationTarget,
    python3_available: &mut Option<bool>,
) -> Result<Option<InstallOutcome>, InstallError> {
    let present = agent_present(paths, target)?;
    if !present {
        tracing::debug!(
            integration = target.label(),
            "agent not present; integration skipped"
        );
        return Ok(None);
    }
    if managed_assets(target).any(|asset| asset.contents.contains("command -v python3")) {
        let python3_available =
            *python3_available.get_or_insert_with(python3_available_on_server_path);
        if !python3_available {
            crate::logging::missing_hook_interpreter(target.label());
        }
    }
    match integration_status(paths, target) {
        Ok(status) if status.state == IntegrationStatusKind::Current => {
            tracing::debug!(integration = target.label(), "integration is current");
            return Ok(None);
        }
        Ok(status) => tracing::debug!(
            integration = status.target.label(),
            path = %status.path.display(),
            state = ?status.state,
            outdated_reason = ?status.outdated_reason,
            "integration needs installation"
        ),
        Err(error) => {
            // Status can reject an old registration that install can repair.
            // Install validates user-owned config again before publishing it.
            tracing::debug!(integration = target.label(), %error, "integration status unavailable; attempting repair");
        }
    }
    install_target(paths, target).map(Some)
}

/// Whether `python3` resolves on the server's own `PATH`. A pane's `PATH` can
/// differ, so this only decides whether to warn.
fn python3_available_on_server_path() -> bool {
    // A refused read cannot happen for a raw variable; treat it as unset.
    let path = shepr_core::env::read_os(shepr_core::env::EnvVar::Path)
        .ok()
        .flatten();
    python3_available_on_path(path.as_deref())
}

fn python3_available_on_path(path: Option<&OsStr>) -> bool {
    let Some(path) = path else {
        return false;
    };
    std::env::split_paths(path).any(|directory| {
        fs::metadata(directory.join("python3"))
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    })
}

pub(crate) fn install_target(
    paths: &AgentIntegrationPaths,
    target: IntegrationTarget,
) -> Result<InstallOutcome, InstallError> {
    // Agent processes do not honor Shepr's config lock. Reload and retry once
    // if the user config changes between the snapshot and publication.
    match install(paths, target) {
        Err(InstallError::ConfigChanged(_)) => install(paths, target),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::python3_available_on_path;

    #[test]
    fn a_status_parse_error_does_not_prevent_repairing_a_managed_kimi_block() {
        use super::{install_if_present, install_target};
        use crate::env::AgentIntegrationPaths;
        use crate::registry::{integration_status, target_directory};
        use crate::types::{InstallErrorKind, IntegrationStatusKind};
        use shepr_agent::IntegrationTarget;

        let _env = shepr_test_support::IsolatedEnv::new();
        let paths = AgentIntegrationPaths::resolve();
        let target = IntegrationTarget::Kimi;
        let directory = target_directory(&paths, target).expect("agent directory");
        fs::create_dir_all(&directory).expect("create agent directory");
        install_target(&paths, target).expect("initial install");
        let config = directory.join(crate::KIMI_CONFIG_NAME);
        fs::write(
            &config,
            format!(
                "user = true\n{}\nbroken = [\n{}\n",
                crate::KIMI_CONFIG_BLOCK_BEGIN,
                crate::KIMI_CONFIG_BLOCK_END,
            ),
        )
        .expect("damage only the managed block");
        let error = integration_status(&paths, target).expect_err("status rejects invalid TOML");
        assert_eq!(error.kind(), InstallErrorKind::ConfigUnparseable);

        let output = install_if_present(&paths, target, &mut Some(true))
            .expect("install can remove and rebuild its own block")
            .expect("repair performed");
        assert!(!output.artifacts.is_empty());
        assert!(
            fs::read_to_string(config)
                .expect("read config")
                .starts_with("user = true\n")
        );
        assert_eq!(
            integration_status(&paths, target)
                .expect("repaired status")
                .state,
            IntegrationStatusKind::Current
        );
    }

    #[test]
    fn python3_probe_uses_the_supplied_path() {
        let scratch = shepr_test_support::ScratchDir::new("hook-python-path");
        let bin = scratch.path().join("bin");
        let empty = scratch.path().join("empty");
        fs::create_dir_all(&bin).expect("test precondition");
        fs::create_dir_all(&empty).expect("test precondition");
        let python3 = bin.join("python3");
        fs::write(&python3, "").expect("test precondition");
        fs::set_permissions(&python3, fs::Permissions::from_mode(0o755))
            .expect("test precondition");

        assert!(python3_available_on_path(Some(bin.as_os_str())));
        assert!(!python3_available_on_path(Some(empty.as_os_str())));
    }
    #[test]
    fn current_integrations_skip_launch_installation_for_every_target() {
        use super::{install_if_present, install_target};
        use crate::env::AgentIntegrationPaths;
        use crate::registry::target_directory;

        let _env = shepr_test_support::IsolatedEnv::new();
        let paths = AgentIntegrationPaths::resolve();
        for target in shepr_agent::IntegrationTarget::all() {
            let directory = target_directory(&paths, target).expect("agent directory");
            fs::create_dir_all(directory).expect("create agent directory");
            install_target(&paths, target).expect("initial install");
            assert!(
                install_if_present(&paths, target, &mut Some(true))
                    .expect("current integration status")
                    .is_none(),
                "{target:?}"
            );
        }
    }
}
