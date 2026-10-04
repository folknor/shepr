mod args;
mod bridge;
mod discovery;
mod failure;
mod host;
mod limits;
pub mod machine;
mod machine_ssh;
mod preflight;
mod process;
mod relay;
mod server_lifecycle;
mod shell_command;
mod ssh;
mod ssh_paths;

pub use args::RemoteCliCommand;
pub use failure::SshFailureDiagnostic;
pub use host::{classified_bridge_failure, run_remote_client_bridge};
pub use limits::SSH_CONNECTION_ATTEMPT_BUDGET;
pub use machine::SshTarget;
pub use machine_ssh::{MachineSshBridge, MachineSshConnector, MachineSshStream};
pub use preflight::{
    AuthenticationError, MachineCheck, MachineSshPreflight, PreflightOutcome, PreflightSsh,
    RestartDecider, classify_check, preflight, restart_different_builds,
};
pub use relay::RemoteBridgeOutcome;
pub use server_lifecycle::{DifferentBuildServer, MachineSshCheck};
pub use shell_command::shell_quote;
pub use ssh::{release_ssh_resources_before_exit, ssh_authentication_command, ssh_check_command};
pub use ssh_paths::validate_remote_bridge_endpoint_path;
