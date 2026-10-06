use super::*;
use shepr_mux::events::{AppEvent, RuntimeGeneration};

/// A production-policy server with one pane whose runtime is live.
fn server_with_runtime_pane(
    name: &str,
) -> (
    HeadlessServer,
    shepr_core::layout::PaneId,
    RuntimeGeneration,
) {
    let mut server = test_headless_server();
    let workspace = shepr_mux::workspace::Workspace::test_new(name);
    let pane_id = workspace.tree().root();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server.app.test_state_mut().seed_bookmark_index(Some(0));

    let runtime = shepr_mux::pane::PaneRuntime::test_with_screen_bytes(80, 24, b"");
    let generation = runtime.generation();
    server.app.insert_test_runtime(pane_id, runtime);
    server.persist_for_test();
    (server, pane_id, generation)
}

/// Delivers a signalled, runtime-tagged exit of the pane the way the loop does.
fn deliver_interrupted_exit(
    server: &mut HeadlessServer,
    pane_id: shepr_core::layout::PaneId,
    generation: RuntimeGeneration,
) {
    server.handle_internal_event_with_forwarding(
        shepr_mux::events::RuntimeEvent::PaneDied {
            ending: shepr_mux::pane::PaneEnding::new(shepr_mux::pane::PaneEndReason::Signalled),
            ended_at: std::time::Instant::now(),
        }
        .enveloped(pane_id, generation),
    );
}

fn server_with_held_runtime_exit() -> (
    HeadlessServer,
    shepr_core::layout::PaneId,
    RuntimeGeneration,
    crate::app::CheckpointGeneration,
) {
    let (mut server, pane_id, generation) = server_with_runtime_pane("checkpointed-runtime-exit");
    deliver_interrupted_exit(&mut server, pane_id, generation);

    assert!(server.app.state().pane(pane_id).is_some());
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
    // Wait on the writer's completion signal (a stored `notify_one` permit, so
    // a completion before the wait is not missed). The bound only fails a
    // wedged writer instead of hanging the test; it is no disk-speed limit.
    let finished = server.outputs.save_finished_signal();
    tokio::time::timeout(crate::test_support::SESSION_WRITE_TEST_BOUND, async {
        while server.app.test_saver().save_in_flight() {
            if !server.app.reap_finished_session_save() {
                finished.notified().await;
            }
        }
    })
    .await
    .expect("the pane-exit checkpoint write finished");
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

    let now = server.app.clock().now;
    server.handle_scheduled_tasks_headless(now);

    assert!(server.pending_checkpointed_pane_exits.is_empty());
    assert!(server.app.state().pane(pane_id).is_none());
    shutdown_test_runtimes(&mut server);
}

/// The checkpoint decision is made once, when the exit is prepared: a core
/// that breaks while the exit waits does not change it, and the replay still
/// removes the pane.
#[tokio::test]
async fn a_core_broken_while_the_exit_waits_still_replays_it() {
    let (mut server, pane_id, _generation, checkpoint_generation) = server_with_held_runtime_exit();
    server.app.test_runtime(pane_id).test_break_terminal_core();

    wait_for_checkpoint(&mut server).await;
    assert!(
        server
            .app
            .pane_exit_checkpoint_generation_settled(checkpoint_generation)
    );
    assert_eq!(server.pending_checkpointed_pane_exits.len(), 1);

    let now = server.app.clock().now;
    server.handle_scheduled_tasks_headless(now);

    assert!(server.pending_checkpointed_pane_exits.is_empty());
    assert!(server.app.state().pane(pane_id).is_none());
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

    let now = server.app.clock().now;
    server.handle_scheduled_tasks_headless(now);

    assert!(server.pending_checkpointed_pane_exits.is_empty());
    assert!(server.app.state().pane(pane_id).is_some());
    assert_eq!(
        server.app.test_runtime(pane_id).generation(),
        replacement_generation
    );
    shutdown_test_runtimes(&mut server);
}

/// An abandoned checkpoint releases its exit, and an autosave that lands
/// before the next scheduled-tasks pass does not hold it again: release is a
/// property of the generation, not a flag that pass must consume.
#[tokio::test]
async fn a_released_exit_is_replayed_by_the_pass_after_the_autosave_that_followed() {
    let (mut server, pane_id, runtime_generation) = server_with_runtime_pane("released-exit");
    // An autosave in flight keeps the exit's checkpoint from starting.
    let autosave = server.app.test_saver().hold_test_save_in_flight();
    deliver_interrupted_exit(&mut server, pane_id, runtime_generation);
    let generation = server
        .pending_checkpointed_pane_exits
        .front()
        .expect("held exit")
        .checkpoint_generation;
    autosave.complete(Ok(()));
    assert!(server.app.reap_finished_session_save());
    for _ in 0..crate::limits::CHECKPOINT_MAX_FAILURES {
        let completion = server
            .app
            .test_saver()
            .hold_test_checkpoint_in_flight(generation);
        completion.complete(Err(std::io::Error::other("disk full").into()));
        assert!(server.app.reap_finished_session_save());
    }
    assert!(
        server
            .app
            .pane_exit_checkpoint_generation_settled(generation)
    );
    let autosave = server.app.test_saver().hold_test_save_in_flight();
    autosave.complete(Ok(()));
    assert!(server.app.reap_finished_session_save());
    assert!(
        server
            .app
            .pane_exit_checkpoint_generation_settled(generation)
    );
    assert_eq!(server.pending_checkpointed_pane_exits.len(), 1);
    assert!(server.app.state().pane(pane_id).is_some());

    server.handle_scheduled_tasks_headless(server.app.clock().now);
    assert!(server.pending_checkpointed_pane_exits.is_empty());
    assert!(server.app.state().pane(pane_id).is_none());
    shutdown_test_runtimes(&mut server);
}

impl HeadlessServer {
    /// App-only fixtures still enter the real server loop for runtime exits.
    pub(crate) fn replay_test_exit_for_app(app: &mut crate::app::App, event: AppEvent) {
        let mut server = test_headless_server();
        std::mem::swap(app, &mut server.app);
        server.handle_test_runtime_exit_and_replay(event);
        std::mem::swap(app, &mut server.app);
    }

    /// Drive an exit through admission, the held queue and scheduled replay.
    pub(crate) fn handle_test_runtime_exit_and_replay(&mut self, event: AppEvent) {
        assert!(
            matches!(event, AppEvent::Runtime { .. }),
            "runtime envelope required"
        );
        self.handle_internal_event_with_forwarding(event);
        // The production reap drives completion and the virtual app clock
        // drives retry deadlines. The wall-clock bound only fails a wedged
        // writer instead of hanging the test; it is no disk-speed limit.
        let started = std::time::Instant::now();
        while !self.pending_checkpointed_pane_exits.is_empty() {
            assert!(
                started.elapsed() < crate::test_support::SESSION_WRITE_TEST_BOUND,
                "checkpoint replay did not finish"
            );
            let now = if self.app.test_saver().save_in_flight() {
                self.app.clock().now
            } else {
                self.app
                    .test_saver()
                    .deadline()
                    .unwrap_or(self.app.clock().now)
            };
            self.app.set_clock(crate::app::AppClock {
                now,
                wall_now: self.app.clock().wall_now,
            });
            self.handle_scheduled_tasks_headless(now);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
