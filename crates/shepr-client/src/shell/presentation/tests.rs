//! The presented surface through the whole shell: how surfaces, patches and snapshots
//! pair into the baseline and the held pair the frame is drawn from.

use crate::endpoint::ClientEndpointId;
use crate::shell::config::ClientShellConfig;
use crate::shell::presentation::surface_patch::{ClientPaneSurfacePatchOutcome, PatchPresentation};
use crate::shell::presentation::surfaces::{PaneSurfaces, PatchRejection};
use crate::shell::state::{ClientShellInput, ClientShellState};
use crate::shell::tests::{snapshot, surface};
use shepr_config::ClientConfig;
use shepr_protocol::{PaneSurfaceFrame, PaneSurfacePatch};

fn state() -> ClientShellState {
    let mut s = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    s.set_snapshot(Box::new(snapshot()));
    s.receive_pane_surface_from(
        surface(),
        s.endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    s
}
fn patch(s: &PaneSurfaceFrame) -> PaneSurfacePatch {
    PaneSurfacePatch {
        boot_id: s.boot_id.clone(),
        projection_revision: s.projection_revision,
        base_surface_revision: s.surface_revision,
        surface_revision: s.surface_revision.checked_next().expect("next"),
        rows: vec![],
        panes: vec![],
        cursor: None,
    }
}
fn changed_patch(surface: &PaneSurfaceFrame, marker: &str) -> PaneSurfacePatch {
    let mut p = patch(surface);
    p.panes.push(surface.panes[0].clone());
    let mut cell = shepr_protocol::CellData::blank();
    cell.symbol = marker.into();
    p.rows.push(shepr_protocol::PaneSurfacePatchRow {
        x: 0,
        y: 0,
        cells: vec![cell],
    });
    p
}
#[test]
fn a_patch_on_a_surface_ahead_of_the_snapshot_advances_that_baseline() {
    let mut s = state();
    let mut next = surface();
    next.projection_revision = shepr_test_fixtures::counter_at(2);
    s.receive_pane_surface_from(
        next,
        s.endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    let p = changed_patch(s.presentation.surfaces.baseline().expect("baseline"), "X");
    assert!(matches!(
        s.apply_pane_surface_patch_from(
            &p,
            s.endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST)
        ),
        ClientPaneSurfacePatchOutcome::Applied(PatchPresentation::Held)
    ));
    assert_eq!(
        s.pane_surface().expect("held").projection_revision,
        shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(1)
    );
    assert_ne!(s.pane_surface().expect("held").frame.cells()[0].symbol, "X");
    let mut next = snapshot();
    next.revision = shepr_test_fixtures::counter_at(2);
    s.set_snapshot(Box::new(next));
    assert_eq!(
        s.pane_surface().expect("paired").frame.cells()[0].symbol,
        "X"
    );
    assert_eq!(
        s.pane_surface().expect("paired").surface_revision,
        p.surface_revision
    );
}
#[test]
fn a_patch_after_the_snapshot_passed_the_surface_advances_the_baseline() {
    let mut s = state();
    let mut next = snapshot();
    next.revision = shepr_test_fixtures::counter_at(2);
    s.set_snapshot(Box::new(next));
    let p = patch(s.presentation.surfaces.baseline().expect("baseline"));
    assert!(matches!(
        s.apply_pane_surface_patch_from(
            &p,
            s.endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST)
        ),
        ClientPaneSurfacePatchOutcome::Applied(PatchPresentation::Held)
    ));
    assert_eq!(
        s.presentation
            .surfaces
            .baseline()
            .expect("baseline")
            .surface_revision,
        p.surface_revision
    );
    assert_ne!(
        s.pane_surface().expect("held").surface_revision,
        p.surface_revision
    );
    let second = changed_patch(s.presentation.surfaces.baseline().expect("baseline"), "X");
    assert!(matches!(
        s.apply_pane_surface_patch_from(
            &second,
            s.endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST)
        ),
        ClientPaneSurfacePatchOutcome::Applied(PatchPresentation::Held)
    ));
    assert_eq!(
        s.presentation
            .surfaces
            .baseline()
            .expect("baseline")
            .frame
            .cells()[0]
            .symbol,
        "X"
    );
    assert_ne!(s.pane_surface().expect("held").frame.cells()[0].symbol, "X");
    let mut next = surface();
    next.projection_revision = shepr_test_fixtures::counter_at(2);
    s.receive_pane_surface_from(
        next,
        s.endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    assert!(s.presentation.surfaces.is_paired());
}

#[test]
fn fast_path_patches_resolve_pane_chrome_roles_before_blitting() {
    let mut s = state();
    s.compose(106, 20).expect("terminal frame");
    let surface = s.presentation.surfaces.baseline().expect("baseline");
    let mut p = changed_patch(surface, "X");
    p.rows[0].cells[0].fg =
        shepr_protocol::WireColor::Chrome(shepr_protocol::ChromeRole::BorderFocused);
    p.rows[0].cells[0].bg =
        shepr_protocol::WireColor::Chrome(shepr_protocol::ChromeRole::ScrollThumbFocused);
    let chrome = crate::shell::view::chrome_palette(&s);

    let ClientPaneSurfacePatchOutcome::Applied(PatchPresentation::Rows(composed)) = s
        .apply_pane_surface_patch_from(
            &p,
            s.endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
        )
    else {
        panic!("unoccluded patch should use the row fast path")
    };

    assert_eq!(
        composed.rows[0].cells[0].fg,
        chrome.resolve(shepr_protocol::WireColor::Chrome(
            shepr_protocol::ChromeRole::BorderFocused,
        ))
    );
    assert_eq!(
        composed.rows[0].cells[0].bg,
        chrome.resolve(shepr_protocol::WireColor::Chrome(
            shepr_protocol::ChromeRole::ScrollThumbFocused,
        ))
    );
}

#[test]
fn a_new_generation_loses_the_baseline_but_keeps_the_held_pair() {
    let mut s = state();
    s.set_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        crate::tests::test_generation(1),
        Box::new(snapshot()),
    );
    s.receive_pane_surface_from(
        surface(),
        s.endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    s.set_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        crate::tests::test_generation(2),
        Box::new(snapshot()),
    );
    assert!(s.presentation.surfaces.baseline().is_none());
    assert!(s.pane_surface().is_some());
}
#[test]
fn a_surface_before_the_first_snapshot_stays_the_baseline() {
    let mut s = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    s.receive_pane_surface_from(surface(), crate::tests::test_generation(1));
    s.set_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        crate::tests::test_generation(1),
        Box::new(snapshot()),
    );
    assert!(s.presentation.surfaces.is_paired());
    let p = patch(s.presentation.surfaces.baseline().expect("baseline"));
    assert!(matches!(
        s.apply_pane_surface_patch_from(&p, crate::tests::test_generation(1)),
        ClientPaneSurfacePatchOutcome::Applied(_)
    ));
}
#[test]
fn a_reconnected_connections_surface_before_its_snapshot_is_kept_and_patched() {
    let mut s = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    s.set_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        crate::tests::test_generation(1),
        Box::new(snapshot()),
    );
    s.receive_pane_surface_from(surface(), crate::tests::test_generation(1));
    assert!(s.presentation.surfaces.is_paired());
    // Same boot and projection revision as the old connection's snapshot: it
    // must wait for its own snapshot rather than pair with the old one.
    s.receive_pane_surface_from(surface(), crate::tests::test_generation(2));
    assert!(!s.presentation.surfaces.is_paired());
    assert!(s.pane_surface().is_some(), "the old pair stays held");
    s.set_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        crate::tests::test_generation(2),
        Box::new(snapshot()),
    );
    assert!(s.presentation.surfaces.is_paired());
    let p = patch(s.presentation.surfaces.baseline().expect("baseline"));
    assert!(matches!(
        s.apply_pane_surface_patch_from(&p, crate::tests::test_generation(2)),
        ClientPaneSurfacePatchOutcome::Applied(_)
    ));
    // A late full surface from the old connection cannot replace it.
    s.receive_pane_surface_from(surface(), crate::tests::test_generation(1));
    assert!(s.presentation.surfaces.is_paired());
}
#[test]
fn a_rebooted_servers_surface_before_its_snapshot_survives_the_reset() {
    let mut s = ClientShellState::new(ClientShellConfig::from_config(&ClientConfig::default()));
    s.set_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        crate::tests::test_generation(1),
        Box::new(snapshot()),
    );
    s.receive_pane_surface_from(surface(), crate::tests::test_generation(1));
    let rebooted = crate::tests::test_boot_id("restarted-local");
    let mut next_surface = surface();
    next_surface.boot_id = rebooted.clone();
    s.receive_pane_surface_from(next_surface, crate::tests::test_generation(2));
    let mut next_snapshot = snapshot();
    next_snapshot.boot_id = rebooted;
    s.set_endpoint_snapshot_for_generation(
        &ClientEndpointId::Local,
        crate::tests::test_generation(2),
        Box::new(next_snapshot),
    );
    assert!(s.presentation.surfaces.is_paired());
    let p = patch(s.presentation.surfaces.baseline().expect("baseline"));
    assert!(matches!(
        s.apply_pane_surface_patch_from(&p, crate::tests::test_generation(2)),
        ClientPaneSurfacePatchOutcome::Applied(_)
    ));
}
#[test]
fn a_slow_path_patch_applies_in_place_and_composes() {
    let mut s = state();
    crate::shell::tests::enter_navigation(&mut s);
    let ptr = s
        .presentation
        .surfaces
        .baseline()
        .expect("baseline")
        .frame
        .cells()
        .as_ptr();
    let p = changed_patch(s.presentation.surfaces.baseline().expect("baseline"), "X");
    assert!(matches!(
        s.apply_pane_surface_patch_from(
            &p,
            s.endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST)
        ),
        ClientPaneSurfacePatchOutcome::Applied(PatchPresentation::Compose)
    ));
    assert_eq!(
        s.presentation
            .surfaces
            .baseline()
            .expect("baseline")
            .frame
            .cells()
            .as_ptr(),
        ptr
    );
    assert_eq!(
        s.pane_surface().expect("paired").frame.cells()[0].symbol,
        "X"
    );
}
#[test]
fn a_rejected_patch_reports_its_reason() {
    let mut s = state();
    let mut p = patch(s.presentation.surfaces.baseline().expect("baseline"));
    p.base_surface_revision = p.surface_revision;
    assert!(matches!(
        s.apply_pane_surface_patch_from(
            &p,
            s.endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST)
        ),
        ClientPaneSurfacePatchOutcome::Rejected(PatchRejection::DoesNotFollow)
    ));
    p = patch(s.presentation.surfaces.baseline().expect("baseline"));
    let mut pane = s.presentation.surfaces.baseline().expect("baseline").panes[0].clone();
    pane.content_rect.width += 1;
    p.panes.push(pane);
    assert!(matches!(
        s.apply_pane_surface_patch_from(
            &p,
            s.endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST)
        ),
        ClientPaneSurfacePatchOutcome::Rejected(PatchRejection::PaneGeometry)
    ));
    p.panes.clear();
    p.rows.push(shepr_protocol::PaneSurfacePatchRow {
        x: 0,
        y: 0,
        cells: vec![],
    });
    assert!(matches!(
        s.apply_pane_surface_patch_from(
            &p,
            s.endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST)
        ),
        ClientPaneSurfacePatchOutcome::Rejected(PatchRejection::RowOutsideFrame)
    ));
    s.presentation.surfaces = PaneSurfaces::Empty;
    assert!(matches!(
        s.apply_pane_surface_patch_from(
            &p,
            s.endpoints
                .active
                .generation()
                .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST)
        ),
        ClientPaneSurfacePatchOutcome::Rejected(PatchRejection::NoBaseline)
    ));
}
#[test]
fn a_surface_ahead_of_the_snapshot_keeps_the_pane_hits_live() {
    let mut s = state();
    s.compose(106, 20).expect("compose");
    let count = s.pane_hits().len();
    assert!(count > 0);
    let mut future = surface();
    future.projection_revision = shepr_test_fixtures::counter_at(3);
    s.receive_pane_surface_from(
        future,
        s.endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    assert_eq!(s.pane_hits().len(), count);
    assert_eq!(
        s.pane_surface().expect("held").projection_revision,
        shepr_test_fixtures::counter_at::<shepr_protocol::ProjectionRevision>(1)
    );
}
#[test]
fn compose_holds_the_last_frame_while_unpaired_and_draws_the_placeholder_when_nothing_was_presented()
 {
    let mut s = state();
    s.compose(106, 20).expect("compose");
    let mut future = surface();
    future.projection_revision = shepr_test_fixtures::counter_at(3);
    s.receive_pane_surface_from(
        future.clone(),
        s.endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    assert!(s.compose(106, 20).is_none());
    s.invalidate_pane_surface();
    s.receive_pane_surface_from(
        future,
        s.endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    assert!(s.compose(106, 20).is_some());
    assert!(s.pane_surface().is_none());
}
#[test]
fn pairing_a_waiting_baseline_runs_the_selection_and_copy_mode_effects() {
    let mut s = state();
    s.compose(106, 20).expect("compose");
    let mut input = ClientShellInput::default();
    s.enter_copy_mode(&mut input);
    let hit = s.pane_hits()[0].clone();
    let metrics = hit.scroll.expect("scroll");
    s.request_word_selection(&hit, metrics, 0, 0, &mut input);
    s.copy.as_mut().expect("copy").cursor.row = shepr_term::AbsRow(0);
    let mut future = surface();
    future.projection_revision = shepr_test_fixtures::counter_at(2);
    future.panes[0].content_rect.width = 10;
    {
        let metrics = future.panes[0].scroll.as_mut().expect("scroll");
        *metrics = shepr_term::ScrollMetrics::new(
            metrics.offset_from_bottom,
            metrics.max_offset_from_bottom,
            metrics.viewport_rows,
            shepr_term::AbsRow(100),
        );
    }
    s.receive_pane_surface_from(
        future,
        s.endpoints
            .active
            .generation()
            .unwrap_or(shepr_protocol::ConnectionGeneration::FIRST),
    );
    assert!(s.mouse_selection.word_gesture.is_some());
    let mut next = snapshot();
    next.revision = shepr_test_fixtures::counter_at(2);
    s.set_snapshot(Box::new(next));
    assert!(s.mouse_selection.word_gesture.is_none());
    assert_ne!(
        s.copy.as_ref().expect("copy").cursor.row,
        shepr_term::AbsRow(0)
    );
}
