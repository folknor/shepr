use crate::endpoint::EndpointSupervisors;
use crate::errors::{ClientExit, ClientRunError};
use crate::launch::run_launched_client;

/// Runs the thin client and enters the main event loop, with fresh connectors
/// for the configured machines: no startup preflight ran before this launch.
///
/// Returns the lines the binary prints once the host terminal is restored; a
/// failed run is a [`ClientRunError`], after which the binary exits nonzero.
pub fn run_client(
    config: &shepr_config::ValidatedClientConfig,
    paths: &shepr_paths::AppPaths,
) -> Result<ClientExit, ClientRunError> {
    let connectors = EndpointSupervisors::fresh_connectors(paths, config.machines());
    run_launched_client(config, paths, connectors)
}

/// Runs the thin client with the connectors startup preflight handed over, one
/// per configured machine, so the transport and remote executable preflight
/// verified are reused instead of resolved again. The preflight must already
/// have run: the client takes the terminal and connects with `BatchMode`.
pub fn run_client_with_connectors(
    config: &shepr_config::ValidatedClientConfig,
    paths: &shepr_paths::AppPaths,
    connectors: Vec<shepr_remote::MachineSshConnector>,
) -> Result<ClientExit, ClientRunError> {
    run_launched_client(config, paths, connectors)
}
