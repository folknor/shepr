use super::*;
use shepr_agent::{Agent, AgentState};
use shepr_mux::events::AppEvent;

#[test]
fn headless_internal_event_drain_is_bounded_per_tick() {
    let mut server = test_headless_server();
    for _ in 0..=crate::limits::APP_EVENT_DRAIN_LIMIT {
        server
            .outputs
            .event_sender()
            .try_send(AppEvent::GitStatusRefreshed {
                outcome: shepr_git::RefreshOutcome::empty(),
            })
            .expect("test precondition");
    }

    assert!(!server.drain_internal_events_with_forwarding());
    assert_eq!(server.outputs.queued_events(), 1);
    assert!(!server.drain_internal_events_with_forwarding());
    assert!(server.outputs.no_queued_events());
    shutdown_test_runtimes(&mut server);
}

#[test]
fn unchanged_git_status_drain_clears_in_flight_without_rendering() {
    let mut server = test_headless_server();
    server.app.test_mark_git_refresh_in_flight();
    server
        .outputs
        .event_sender()
        .try_send(AppEvent::GitStatusRefreshed {
            outcome: shepr_git::RefreshOutcome::empty(),
        })
        .expect("test precondition");

    assert!(!server.drain_internal_events_with_forwarding());
    assert!(!server.app.git_refresh_in_flight());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn full_internal_event_queue_eventually_applies_working_to_idle_transition() {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("test");
    let pane_id = workspace.tree().root();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);

    server.app.insert_idle_test_runtime(pane_id);
    let now = server.app.clock().now;
    let working = server.app.from_pane_runtime(
        pane_id,
        shepr_mux::events::RuntimeEvent::StateChanged {
            agent: Some(Agent::Pi),
            detection: shepr_detect::Detection::new(AgentState::Working, false),
            process_exited: false,
            observed_at: now,
        },
    );
    server.app.handle_internal_event(working);
    assert_eq!(
        server
            .app
            .state()
            .pane(pane_id)
            .expect("pane exists")
            .terminal()
            .ownership()
            .state(),
        AgentState::Working
    );

    for _ in 0..crate::limits::APP_EVENT_CHANNEL_CAPACITY {
        server
            .outputs
            .event_sender()
            .try_send(AppEvent::GitStatusRefreshed {
                outcome: shepr_git::RefreshOutcome::empty(),
            })
            .expect("test precondition");
    }

    let tx = server.outputs.event_sender();
    let now = server.app.clock().now;
    let send = tx.send(server.app.from_pane_runtime(
        pane_id,
        shepr_mux::events::RuntimeEvent::StateChanged {
            agent: Some(Agent::Pi),
            detection: shepr_detect::Detection::new(AgentState::Idle, false),
            process_exited: false,
            observed_at: now,
        },
    ));
    tokio::pin!(send);

    let blocked =
        tokio::time::timeout(Duration::from_millis(20), async { (&mut send).await }).await;
    assert!(
        blocked.is_err(),
        "state change sender should wait for queue space instead of failing"
    );

    server.drain_internal_events_with_forwarding();

    tokio::time::timeout(Duration::from_millis(50), async { (&mut send).await })
        .await
        .expect("state change should enqueue once queue space is available")
        .expect("app event receiver should still be alive");

    let max_drains =
        (crate::limits::APP_EVENT_CHANNEL_CAPACITY / crate::limits::APP_EVENT_DRAIN_LIMIT) + 2;
    for _ in 0..max_drains {
        if server
            .app
            .state()
            .pane(pane_id)
            .expect("pane exists")
            .terminal()
            .ownership()
            .state()
            == AgentState::Idle
        {
            break;
        }
        server.drain_internal_events_with_forwarding();
    }

    assert_eq!(
        server
            .app
            .state()
            .pane(pane_id)
            .expect("pane exists")
            .terminal()
            .ownership()
            .state(),
        AgentState::Idle,
        "Working to Idle should still apply after temporary queue pressure"
    );
    shutdown_test_runtimes(&mut server);
}
