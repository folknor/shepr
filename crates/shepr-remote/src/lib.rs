mod args;
mod bridge;
mod discovery;
mod failure;
pub mod fleet;
mod host;
mod limits;
pub mod machine;
mod machine_ssh;
mod preflight;
mod process;
mod relay;
mod server_lifecycle;
mod server_wait;
mod shell_command;
mod ssh;
mod ssh_paths;

pub use args::RemoteCliCommand;
pub use host::{BridgeMode, classified_bridge_failure, run_remote_client_bridge};
pub use limits::{
    SSH_CONNECTION_ATTEMPT_BUDGET, SSH_RESTART_ATTEMPT_BUDGET, SSH_START_ATTEMPT_BUDGET,
    SSH_START_BRIDGE_BUDGET,
};
pub use machine::SshTarget;
pub use machine_ssh::{ConnectMode, MachineSshConnection, MachineSshConnector, ServerWatchEnd};
pub use preflight::{AuthenticationError, MachineCheck, MachineSshPreflight, PreflightOutcome};
pub use relay::RemoteBridgeOutcome;
pub use server_wait::{ServerWaitEnd, wait_for_server};
pub use ssh::{release_ssh_resources_before_exit, ssh_authentication_command, ssh_check_command};
