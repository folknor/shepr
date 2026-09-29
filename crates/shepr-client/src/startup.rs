use super::*;

/// Runs the thin client and enters the main event loop.
///
/// Returns the lines the binary prints once the host terminal is restored; a
/// failed run is a [`ClientRunError`], after which the binary exits nonzero.
pub fn run_client(
    config: &shepr_config::ValidatedConfig,
    paths: &shepr_config::AppPaths,
) -> Result<ClientExit, ClientRunError> {
    run_launched_client(config, paths)
}
