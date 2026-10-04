use super::endpoint_choice::{Fixture, snapshot, surface};
use super::*;
use endpoint::ClientEndpointId;
use shepr_protocol::ServerMessage;

fn patch(s: &shepr_protocol::PaneSurfaceFrame) -> shepr_protocol::PaneSurfacePatch {
    shepr_protocol::PaneSurfacePatch {
        boot_id: s.boot_id.clone(),
        projection_revision: s.projection_revision,
        base_surface_revision: s.surface_revision,
        surface_revision: s.surface_revision.checked_next().expect("successor"),
        rows: vec![],
        panes: vec![],
        cursor: None,
    }
}
fn assert_connected(f: &Fixture) {
    assert!(
        f.client
            .hub()
            .registry()
            .connection(&ClientEndpointId::Local)
            .is_some(),
        "patch must preserve the connection"
    );
}
#[test]
fn a_patch_on_a_surface_ahead_of_its_snapshot_does_not_fail_the_connection() {
    let mut f = Fixture::new();
    let s = surface(&ClientEndpointId::Local, 2, f.size(), "FUTURE");
    f.inbound(
        &ClientEndpointId::Local,
        ServerMessage::PaneSurface(s.clone()),
    );
    f.inbound_patch(&ClientEndpointId::Local, patch(&s));
    assert_connected(&f);
}
#[test]
fn a_patch_during_a_projection_gap_does_not_fail_the_connection() {
    let mut f = Fixture::new();
    let mut s = surface(&ClientEndpointId::Local, 1, f.size(), "SOURCE");
    f.inbound(
        &ClientEndpointId::Local,
        ServerMessage::EndpointSnapshot(snapshot(&ClientEndpointId::Local, 2)),
    );
    let first = patch(&s);
    s.surface_revision = first.surface_revision;
    f.inbound_patch(&ClientEndpointId::Local, first);
    f.inbound_patch(&ClientEndpointId::Local, patch(&s));
    assert_connected(&f);
}

#[test]
fn a_patch_that_does_not_follow_its_baseline_fails_the_connection() {
    let mut f = Fixture::new();
    let s = surface(&ClientEndpointId::Local, 2, f.size(), "WRONG");
    f.inbound_patch(&ClientEndpointId::Local, patch(&s));
    assert!(
        f.client
            .hub()
            .registry()
            .connection(&ClientEndpointId::Local)
            .is_none()
    );
}
#[test]
fn the_commit_baseline_is_the_evidence_surface_and_the_next_patch_applies() {
    let mut f = Fixture::new();
    f.start();
    f.evidence();
    f.reconcile();
    let id = super::endpoint_choice::remote();
    assert!(
        f.client.state().shell.endpoint_is_active(&id),
        "the move committed"
    );
    let mut s = surface(&id, 2, f.size(), "TARGET");
    let first = patch(&s);
    s.surface_revision = first.surface_revision;
    f.inbound_patch(&id, first);
    assert!(f.client.hub().registry().connection(&id).is_some());
    // The first patch reached the shell and advanced its baseline: the next one follows.
    // The committed endpoint's connection generation (the fixture connects it at 7) is
    // the one its patches carry.
    assert!(matches!(
        f.client
            .state_mut()
            .shell
            .apply_pane_surface_patch_from(&patch(&s), crate::tests::test_generation(7)),
        shell::ClientPaneSurfacePatchOutcome::Applied(_)
    ));
}
