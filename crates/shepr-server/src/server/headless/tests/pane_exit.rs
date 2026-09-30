use super::*;
use crate::app::AppPolicy;
use shepr_mux::events::{AppEvent, RuntimeGeneration};

fn server_with_held_runtime_exit() -> (
    HeadlessServer,
    shepr_core::layout::PaneId,
    RuntimeGeneration,
    u64,
) {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new("checkpointed-runtime-exit");
    let pane_id = workspace.root_pane();
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.set_bookmark_index(Some(0));

    let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"");
    let generation = runtime.generation();
    server.app.insert_test_runtime(pane_id, runtime);
    server.app.policy = AppPolicy::Production;

    server.handle_internal_event_with_forwarding(AppEvent::Runtime {
        pane_id,
        generation,
        event: Box::new(AppEvent::PaneDied {
            pane_id,
            exit_reason: shepr_platform::ChildExitReason::Interrupted,
        }),
    });

    assert!(server.app.find_pane(pane_id).is_some());
    let held = server
        .pending_checkpointed_pane_exits
        .front()
        .expect("runtime-tagged pane exit should wait for its checkpoint");
    assert!(matches!(
        &held.event,
        AppEvent::Runtime {
            pane_id: held_pane_id,
            generation: held_generation,
            ..
        } if *held_pane_id == pane_id && *held_generation == generation
    ));
    let checkpoint_generation = held.checkpoint_generation;

    (server, pane_id, generation, checkpoint_generation)
}

async fn wait_for_checkpoint(server: &mut HeadlessServer) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if server.app.reap_finished_session_save() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("pane-exit checkpoint should finish");
}

#[tokio::test]
async fn live_runtime_exit_is_replayed_after_its_checkpoint() {
    let (mut server, pane_id, _generation, checkpoint_generation) = server_with_held_runtime_exit();

    wait_for_checkpoint(&mut server).await;
    assert!(
        server
            .app
            .pane_exit_checkpoint_generation_settled(checkpoint_generation)
    );
    assert_eq!(server.pending_checkpointed_pane_exits.len(), 1);

    let now = server.app.clock.now;
    server.handle_scheduled_tasks_headless(now);

    assert!(server.pending_checkpointed_pane_exits.is_empty());
    assert!(server.app.find_pane(pane_id).is_none());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn replaced_runtime_drops_a_checkpointed_stale_exit_on_replay() {
    let (mut server, pane_id, generation, checkpoint_generation) = server_with_held_runtime_exit();

    wait_for_checkpoint(&mut server).await;
    assert!(
        server
            .app
            .pane_exit_checkpoint_generation_settled(checkpoint_generation)
    );

    let replacement = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"");
    let replacement_generation = replacement.generation();
    assert_ne!(generation, replacement_generation);
    server.app.insert_test_runtime(pane_id, replacement);

    let now = server.app.clock.now;
    server.handle_scheduled_tasks_headless(now);

    assert!(server.pending_checkpointed_pane_exits.is_empty());
    assert!(server.app.find_pane(pane_id).is_some());
    assert_eq!(
        server.app.test_runtime(pane_id).generation(),
        replacement_generation
    );
    shutdown_test_runtimes(&mut server);
}
