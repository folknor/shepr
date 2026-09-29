use shepr_protocol::command::{
    ClientShellSurfaceSetParams, EndpointCommand, EndpointReply, PaneTarget, WorkspaceTarget,
};

use super::super::{ClientEndpointId, EndpointRegistry, EndpointSendOutcome};
use super::model::{ActivationEvidence, EndpointLease};

pub(super) fn focus_result_matches(
    focus: Option<&crate::shell::ClientEndpointFocusTarget>,
    result: &EndpointReply,
) -> bool {
    match (focus, result) {
        (
            Some(crate::shell::ClientEndpointFocusTarget::Pane(expected)),
            EndpointReply::PaneInfo { pane },
        ) => pane.focused && &pane.pane_id == expected,
        (
            Some(crate::shell::ClientEndpointFocusTarget::Workspace(expected)),
            EndpointReply::WorkspaceInfo { workspace },
        ) => workspace.focused && &workspace.workspace_id == expected,
        _ => false,
    }
}

pub(super) fn endpoint_lease(
    shell: &crate::shell::ClientShellState,
    endpoints: &EndpointRegistry,
    endpoint_id: &ClientEndpointId,
) -> Result<EndpointLease, String> {
    let connection = endpoints
        .connection(endpoint_id)
        .ok_or_else(|| "endpoint connection is unavailable".to_owned())?;
    let (boot_id, minimum_revision) = shell
        .endpoint_snapshot_identity(endpoint_id, connection.generation.get())
        .ok_or_else(|| "endpoint metadata is not ready for this connection".to_owned())?;
    Ok(EndpointLease {
        endpoint_id: endpoint_id.clone(),
        generation: connection.generation.get(),
        boot_id: Some(boot_id.clone()),
        minimum_revision,
    })
}

pub(super) fn disconnected_endpoint_lease(
    shell: &crate::shell::ClientShellState,
    endpoint_id: &ClientEndpointId,
) -> EndpointLease {
    EndpointLease {
        endpoint_id: endpoint_id.clone(),
        generation: 0,
        boot_id: shell.endpoint_boot_id(endpoint_id).cloned(),
        minimum_revision: 0,
    }
}

pub(super) fn endpoint_matches(
    lease: &EndpointLease,
    endpoint_id: &ClientEndpointId,
    generation: u64,
    boot_id: &str,
) -> bool {
    lease.endpoint_id == *endpoint_id
        && lease.generation == generation
        && lease
            .boot_id
            .as_ref()
            .is_some_and(|lease_boot_id| lease_boot_id == boot_id)
}

pub(super) fn coherent_completion_surface(
    shell: &crate::shell::ClientShellState,
    lease: &EndpointLease,
    evidence: &ActivationEvidence,
    acknowledgement_revision: Option<u64>,
    geometry: shepr_protocol::ClientSurfaceSize,
) -> Result<shepr_protocol::PaneSurfaceFrame, String> {
    let acknowledgement_revision = acknowledgement_revision.ok_or_else(|| {
        "endpoint activation completed without a surface acknowledgement".to_owned()
    })?;
    let surface = evidence
        .surface
        .clone()
        .ok_or_else(|| "endpoint activation completed without a surface".to_owned())?;
    if surface.projection_revision < acknowledgement_revision
        || !surface_matches_geometry(&surface, geometry)
    {
        return Err("endpoint activation lost its acknowledged surface evidence".into());
    }
    if !shell.endpoint_snapshot_matches(
        &lease.endpoint_id,
        lease.generation,
        lease.request_boot_id()?,
        surface.projection_revision.get(),
    ) {
        return Err("endpoint activation lost its coherent snapshot/surface pair".into());
    }
    Ok(surface)
}

pub(super) fn resize_geometry(
    message: &shepr_protocol::ClientMessage,
) -> Option<shepr_protocol::ClientSurfaceSize> {
    match message {
        shepr_protocol::ClientMessage::ClientShellResize { geometry } => {
            Some(geometry.surface_size())
        }
        _ => None,
    }
}

pub(super) fn surface_matches_geometry(
    surface: &shepr_protocol::PaneSurfaceFrame,
    geometry: shepr_protocol::ClientSurfaceSize,
) -> bool {
    surface.frame.width == geometry.cols && surface.frame.height == geometry.rows
}

pub(super) fn send_surface_activation(
    endpoints: &mut EndpointRegistry,
    target: &EndpointLease,
    request_id: &shepr_protocol::RequestId,
    resize: &shepr_protocol::ClientMessage,
    focused: bool,
) -> Result<(), String> {
    let request = surface_interest_request(target.request_boot_id()?, request_id, true);
    if endpoints.send_to(&target.endpoint_id, resize) != EndpointSendOutcome::Sent {
        return Err("endpoint resize could not be sent".into());
    }
    if endpoints.send_to(&target.endpoint_id, &request) != EndpointSendOutcome::Sent {
        return Err("endpoint activation could not be sent".into());
    }
    // Inactive endpoints reject focus events. Activate first, then establish the host baseline
    // on the same ordered transport before navigation or presentation can commit.
    if endpoints.send_to(
        &target.endpoint_id,
        &shepr_protocol::ClientMessage::ClientShellFocus { focused },
    ) != EndpointSendOutcome::Sent
    {
        return Err("endpoint focus baseline could not be sent".into());
    }
    Ok(())
}

pub(super) fn surface_set_revision(
    result: &EndpointReply,
    expected_active: bool,
) -> Result<u64, String> {
    match result {
        EndpointReply::ClientShellSurfaceSet {
            active,
            projection_revision,
        } if *active == expected_active => Ok(*projection_revision),
        _ => Err("surface activation returned an invalid acknowledgement".into()),
    }
}

pub(super) fn focus_request(
    boot_id: &shepr_protocol::BootId,
    request_id: &shepr_protocol::RequestId,
    focus: &crate::shell::ClientEndpointFocusTarget,
) -> shepr_protocol::ClientMessage {
    let command = match focus {
        crate::shell::ClientEndpointFocusTarget::Workspace(workspace_id) => {
            EndpointCommand::WorkspaceFocus(WorkspaceTarget {
                workspace_id: workspace_id.to_string(),
            })
        }
        crate::shell::ClientEndpointFocusTarget::Pane(pane_id) => {
            EndpointCommand::PaneFocus(PaneTarget {
                pane_id: pane_id.to_string(),
            })
        }
    };
    endpoint_request(boot_id, request_id, command)
}

pub(super) fn surface_interest_request(
    boot_id: &shepr_protocol::BootId,
    request_id: &shepr_protocol::RequestId,
    active: bool,
) -> shepr_protocol::ClientMessage {
    endpoint_request(
        boot_id,
        request_id,
        EndpointCommand::ClientShellSurfaceSet(ClientShellSurfaceSetParams { active }),
    )
}

fn endpoint_request(
    boot_id: &shepr_protocol::BootId,
    request_id: &shepr_protocol::RequestId,
    command: EndpointCommand,
) -> shepr_protocol::ClientMessage {
    shepr_protocol::ClientMessage::ClientShellEndpointRequest {
        boot_id: boot_id.clone(),
        request_id: request_id.clone(),
        command,
    }
}
