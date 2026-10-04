mod dispatch;

use crate::endpoint::{HostBaseline, HubEffect};
use crate::errors::LoopExit;
use crate::events::{ClientLoopEvent, ParsedHostInput};
use crate::shell::ClientShellState;
use crate::shell_runtime::{
    ShellInputDisposition, clear_endpoint_host_effects, finish_client_shell_input, resize_views,
    sync_client_shell_keyboard_report_all, view_geometry,
};
use crate::state::ClientState;
use crate::terminal_geometry::{
    AtomicCellSize, reported_cell_size_from_events, store_reported_cell_size,
};
use crate::{endpoint, fatal_panic, terminal_geometry};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientLoopAction {
    NextEvent,
    Exit,
}

pub(crate) enum ClientLoopWake {
    Event(ClientLoopEvent),
    Deadline,
    FatalPanic,
    QueueClosed,
}

/// The two flags that end the loop from outside it.
pub(crate) struct LoopSignals {
    pub(crate) should_quit: Arc<AtomicBool>,
    /// Checked between each step of an iteration and on every way out: once
    /// a panic is latched the loop starts no further step and returns. A step
    /// already under way (a synchronous terminal write) finishes first.
    pub(crate) fatal: Arc<fatal_panic::FatalPanic>,
}

/// The queue host helpers, endpoint readers, supervisors and the signal handler send on, and
/// the loop's end of it.
pub(crate) struct EventQueue {
    pub(crate) tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    pub(crate) rx: tokio::sync::mpsc::Receiver<ClientLoopEvent>,
}

/// The host's cell size as it reports it, and whether launch asked it to.
pub(crate) struct HostCellReport {
    pub(crate) size: Arc<AtomicCellSize>,
    pub(crate) queried: crate::input::ProbeAvailability,
}

pub(crate) struct ClientLoop {
    state: ClientState,
    /// Connections, supervisors, command lanes and the endpoint move; the shell's endpoint
    /// choice decides only what is shown.
    hub: endpoint::EndpointHub,
    signals: LoopSignals,
    events: EventQueue,
    cell: HostCellReport,
}

fn earliest_client_timer_deadline(
    deadlines: impl IntoIterator<Item = Option<std::time::Instant>>,
) -> Option<std::time::Instant> {
    deadlines.into_iter().flatten().min()
}

async fn wait_for_client_timer(deadline: Option<std::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

impl ClientLoop {
    /// The one construction launch and the loop tests share. It takes
    /// what the caller has already wired up (the event channel host helpers,
    /// endpoint readers, supervisors and the signal handler share, the cell size,
    /// the endpoints) and starts the loop's own state itself, so a test drives
    /// a loop that begins exactly as production's does.
    pub(crate) fn new(
        state: ClientState,
        hub: endpoint::EndpointHub,
        signals: LoopSignals,
        events: EventQueue,
        cell: HostCellReport,
    ) -> Self {
        Self {
            state,
            hub,
            signals,
            events,
            cell,
        }
    }

    fn next_timer_deadline(&mut self, now: std::time::Instant) -> Option<std::time::Instant> {
        earliest_client_timer_deadline([
            self.state.shell.next_timer_deadline(),
            self.hub.next_deadline(&self.state.shell, now),
            self.state.refused_output_retry_deadline(),
        ])
    }

    /// Returns a quit request at once, else waits for the timer armed from the earliest
    /// pending deadline as of `now` or the shared event queue.
    async fn wait_for_next_event(&mut self, now: std::time::Instant) -> ClientLoopWake {
        let timer_deadline = self.next_timer_deadline(now);
        if self.signals.should_quit.load(Ordering::Acquire) {
            return ClientLoopWake::Event(ClientLoopEvent::Quit);
        }

        tokio::select! {
            biased;
            // Keep wake reasons distinct: a panic and a closed queue are not elapsed timers.
            () = self.signals.fatal.latched() => ClientLoopWake::FatalPanic,
            _ = wait_for_client_timer(timer_deadline) => ClientLoopWake::Deadline,
            ev = self.events.rx.recv() => match ev {
                Some(event) => ClientLoopWake::Event(event),
                None => ClientLoopWake::QueueClosed,
            },
        }
    }

    pub(crate) async fn run(&mut self) -> Result<(), LoopExit> {
        let result = self.run_until_exit().await;
        // Detach and terminal loss return through the same event path as Quit;
        // flush regardless of which condition ended the loop.
        self.state.output_writer.flush().ok();
        // Every way out, an error included, reports a latched panic instead.
        if self.signals.fatal.is_latched() {
            return Err(LoopExit::Panicked);
        }
        result
    }

    async fn run_until_exit(&mut self) -> Result<(), LoopExit> {
        while !self.signals.should_quit.load(Ordering::Acquire) {
            if self.signals.fatal.is_latched() {
                return Err(LoopExit::Panicked);
            }
            // client-clock-sample-ok: the pre-wait sample for supervisors and timers.
            let loop_now = std::time::Instant::now();
            self.reconcile(loop_now)?;
            if self.signals.fatal.is_latched() {
                return Err(LoopExit::Panicked);
            }
            let host_geometry = self.state.reported_geometry;
            let shell = &self.state.shell;
            let mouse_capture = self.state.host_modes.mouse_shell_preference();
            self.hub.spawn_due(
                loop_now,
                || endpoint::EndpointConnectOptions {
                    geometry: view_geometry(
                        host_geometry,
                        shell.surface_size(host_geometry.cols(), host_geometry.rows()),
                    ),
                    mouse_capture,
                },
                &self.events.tx,
            );
            let wake = self.wait_for_next_event(loop_now).await;
            if self.signals.fatal.is_latched() {
                return Err(LoopExit::Panicked);
            }
            let event = match wake {
                ClientLoopWake::Event(event) => event,
                ClientLoopWake::Deadline => ClientLoopEvent::Timer,
                ClientLoopWake::FatalPanic => return Err(LoopExit::Panicked),
                ClientLoopWake::QueueClosed => return Ok(()),
            };
            // client-clock-sample-ok: sample after waiting for the event to arrive.
            let now = std::time::Instant::now();
            if self.handle_event(event, now)? == ClientLoopAction::Exit {
                break;
            }
        }
        Ok(())
    }

    pub(crate) fn handle_event(
        &mut self,
        event: ClientLoopEvent,
        now: std::time::Instant,
    ) -> Result<ClientLoopAction, LoopExit> {
        self.state.shell.now = now;
        let action = match event {
            ClientLoopEvent::Quit => Ok(ClientLoopAction::Exit),
            ClientLoopEvent::StdinInput(inputs) => self.handle_stdin_input(inputs, now),
            ClientLoopEvent::TerminalUnavailable(err) => self.handle_terminal_unavailable(&err),
            ClientLoopEvent::Resize(geometry) => self.handle_resize(geometry),
            ClientLoopEvent::EndpointSupervisor(event) => {
                self.handle_endpoint_supervisor(event, now)
            }
            ClientLoopEvent::ServerMessage {
                endpoint_id,
                generation,
                message,
            } => self.handle_server_message(&endpoint_id, generation, message, now),
            ClientLoopEvent::ServerDisconnected {
                endpoint_id,
                generation,
                error,
            } => self.handle_server_disconnected(&endpoint_id, generation, &error),
            ClientLoopEvent::Timer => self.handle_timer(now),
        }?;
        if action == ClientLoopAction::NextEvent {
            if self.state.retry_host_modes {
                self.state.retry_host_modes = false;
                let mouse = self.state.host_modes.reassert_mouse(
                    &mut self.state.output_writer,
                    self.state.reported_geometry.cell(),
                );
                self.state
                    .record_host_mode_write("mouse mode retry", mouse)?;
                sync_client_shell_keyboard_report_all(&mut self.state)?;
            }
            self.state.present_pending();
        }
        Ok(action)
    }

    /// Runs the hub's derivation of what is shown and which connections are viewed once per
    /// iteration before the loop waits, applies the host effects it asks for and presents
    /// what they dirtied.
    pub(crate) fn reconcile(&mut self, now: std::time::Instant) -> Result<(), LoopExit> {
        let host_geometry = self.state.reported_geometry;
        let theme = &self.state.host_theme_updates;
        let baseline = |shell: &ClientShellState| HostBaseline {
            geometry: view_geometry(
                host_geometry,
                shell.surface_size(host_geometry.cols(), host_geometry.rows()),
            ),
            theme,
        };
        let effects = self.hub.reconcile(&mut self.state.shell, baseline, now)?;
        self.apply_hub_effects(effects)?;
        self.state.present_pending();
        Ok(())
    }

    /// Carries out what the hub asked of the host, in the order it asked.
    fn apply_hub_effects(&mut self, effects: Vec<HubEffect>) -> Result<(), LoopExit> {
        for effect in effects {
            match effect {
                HubEffect::Notice(notice) => self.state.present_notice(&notice),
                HubEffect::ClearHostEffects => clear_endpoint_host_effects(&mut self.state)?,
                HubEffect::ChromeDirty => self.state.mark_chrome_dirty(),
                HubEffect::Committed => {
                    self.state.request_repaint();
                    self.state.mark_pane_dirty();
                }
            }
        }
        Ok(())
    }

    fn handle_stdin_input(
        &mut self,
        inputs: Vec<ParsedHostInput>,
        now: std::time::Instant,
    ) -> Result<ClientLoopAction, LoopExit> {
        let Self {
            state, hub, cell, ..
        } = self;
        let raw_events = inputs.iter().map(|input| &input.event);
        if cell.queried == crate::input::ProbeAvailability::Armed
            && let Some(reported) = reported_cell_size_from_events(raw_events)
        {
            store_reported_cell_size(&cell.size, reported);
        }
        if shepr_termio::input::raw_input::events_require_host_mode_refresh(
            inputs.iter().map(|input| &input.event),
        ) && let Err(error) = state
            .host_modes
            .reassert_mouse(&mut state.output_writer, state.reported_geometry.cell())
        {
            // Reassertion repeats a mode the host already accepted after a host event that
            // may have reset it; a failure here is logged and never ends the session.
            warn!(%error, "failed to re-assert host mouse capture");
        }
        let host_reports_all_keys = state.host_modes.keyboard_report_all_active();
        let shell = &mut state.shell;
        let outcome = shell.handle_host_input(inputs, host_reports_all_keys, now);
        if finish_client_shell_input(state, outcome, hub, now)? == ShellInputDisposition::Detach {
            return Ok(ClientLoopAction::Exit);
        }
        Ok(ClientLoopAction::NextEvent)
    }

    fn handle_terminal_unavailable(
        &mut self,
        err: &io::Error,
    ) -> Result<ClientLoopAction, LoopExit> {
        info!(error = %err, "client terminal unavailable; detaching");
        Ok(ClientLoopAction::Exit)
    }

    fn handle_resize(
        &mut self,
        geometry: terminal_geometry::TerminalGeometry,
    ) -> Result<ClientLoopAction, LoopExit> {
        let Self { state, hub, .. } = self;
        let geometry = terminal_geometry::bounded_cell_geometry(geometry);
        state.reported_geometry = geometry;
        state.shell.set_host_cell(geometry.cell());
        let mouse = state
            .host_modes
            .apply_mouse(&mut state.output_writer, geometry.cell());
        state.record_host_mode_write("mouse mode resize", mouse)?;
        // Resizing invalidates the host-side blit baseline. The retained pane surface
        // stays: until the resized one arrives, `compose_frame` draws it clipped to the new
        // pane area (with pane hits clipped to match) instead of dropping to the
        // machine-list placeholder.
        state.request_repaint();
        resize_views(state, hub);
        // The host has already reflowed the old frame. Compose the retained pane surface
        // clipped to the new size at the end of this event, rather than waiting for the next
        // input or server surface.
        state.mark_pane_dirty();
        Ok(ClientLoopAction::NextEvent)
    }

    fn handle_endpoint_supervisor(
        &mut self,
        event: endpoint::EndpointSupervisorEvent,
        now: std::time::Instant,
    ) -> Result<ClientLoopAction, LoopExit> {
        let effects = self.hub.supervisor_event(&mut self.state.shell, event, now);
        self.apply_hub_effects(effects)?;
        Ok(ClientLoopAction::NextEvent)
    }

    fn handle_server_disconnected(
        &mut self,
        endpoint_id: &endpoint::ClientEndpointId,
        generation: shepr_protocol::ConnectionGeneration,
        error: &io::Error,
    ) -> Result<ClientLoopAction, LoopExit> {
        self.hub.disconnected(endpoint_id, generation, error);
        Ok(ClientLoopAction::NextEvent)
    }

    fn handle_timer(&mut self, now: std::time::Instant) -> Result<ClientLoopAction, LoopExit> {
        let Self { state, hub, .. } = self;
        // Presented after this event with everything else it dirtied.
        state.retry_refused_output(now);
        hub.tick_health(now);
        let mut outcome = state.shell.tick_timers(now);
        outcome.merge(hub.settle_expired(&mut state.shell, now));
        if finish_client_shell_input(state, outcome, hub, now)? == ShellInputDisposition::Detach {
            return Ok(ClientLoopAction::Exit);
        }
        Ok(ClientLoopAction::NextEvent)
    }
}

#[cfg(test)]
impl ClientLoop {
    pub(crate) fn state(&self) -> &ClientState {
        &self.state
    }

    pub(crate) fn state_mut(&mut self) -> &mut ClientState {
        &mut self.state
    }

    pub(crate) fn hub(&self) -> &endpoint::EndpointHub {
        &self.hub
    }

    pub(crate) fn hub_mut(&mut self) -> &mut endpoint::EndpointHub {
        &mut self.hub
    }

    /// Both at once, for a call that takes the state and the hub together.
    pub(crate) fn parts_mut(&mut self) -> (&mut ClientState, &mut endpoint::EndpointHub) {
        (&mut self.state, &mut self.hub)
    }
}

#[cfg(test)]
mod client_timer_tests {
    use super::*;
    use shepr_protocol::ClientMessage;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    struct TimerTransport(Arc<Mutex<Vec<ClientMessage>>>);

    impl endpoint::EndpointTransport for TimerTransport {
        fn send(&mut self, message: &ClientMessage) -> io::Result<()> {
            self.0
                .lock()
                .map_err(|_| io::Error::other("test precondition: lock poisoned"))?
                .push(message.clone());
            Ok(())
        }

        fn disconnect(&mut self) {}

        fn flush(&mut self, _deadline: Instant) -> io::Result<()> {
            Ok(())
        }

        fn take_error(&mut self) -> Option<io::Error> {
            None
        }
    }

    fn test_client_loop(
        write_stream: endpoint::EndpointRegistry,
    ) -> (ClientLoop, tokio::sync::mpsc::Sender<ClientLoopEvent>) {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(1);
        (
            ClientLoop::new(
                ClientState::test_new(),
                endpoint::EndpointHub::for_registry(write_stream),
                LoopSignals {
                    should_quit: Arc::new(AtomicBool::new(false)),
                    fatal: Arc::default(),
                },
                EventQueue {
                    tx: event_tx.clone(),
                    rx: event_rx,
                },
                HostCellReport {
                    size: Arc::new(AtomicCellSize::new()),
                    queried: crate::input::ProbeAvailability::NotArmed,
                },
            ),
            event_tx,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn a_panic_latched_elsewhere_ends_a_waiting_loop() {
        let (mut client_loop, _event_tx) = test_client_loop(endpoint::EndpointRegistry::empty());
        let fatal = Arc::clone(&client_loop.signals.fatal);
        let run = client_loop.run();
        tokio::pin!(run);
        assert!(
            tokio::time::timeout(Duration::from_secs(60), &mut run)
                .await
                .is_err(),
            "an idle loop keeps running"
        );
        fatal.latch();
        assert!(matches!(run.await, Err(LoopExit::Panicked)));
    }

    /// A host helper thread can panic after launch checked the latch and
    /// before the loop first waits; the loop ends without handling an event.
    #[tokio::test(start_paused = true)]
    async fn a_panic_latched_before_the_loop_starts_ends_it_at_once() {
        let (mut client_loop, event_tx) = test_client_loop(endpoint::EndpointRegistry::empty());
        client_loop.signals.fatal.latch();
        event_tx
            .try_send(ClientLoopEvent::Quit)
            .expect("test precondition");
        assert!(matches!(client_loop.run().await, Err(LoopExit::Panicked)));
        assert!(
            client_loop.events.rx.try_recv().is_ok(),
            "the queued event was never handled"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn quit_event_wakes_a_deadline_free_loop() {
        let now = tokio::time::Instant::now().into_std();
        let (mut client_loop, event_tx) = test_client_loop(endpoint::EndpointRegistry::empty());
        // The wait future borrows the loop, so it lives in its own scope and
        // the loop is free again to handle the event it produced.
        let wake = {
            let wait = client_loop.wait_for_next_event(now);
            tokio::pin!(wait);
            // Poll once so the loop is parked on its queue before quit arrives.
            assert!(
                tokio::time::timeout(Duration::from_secs(60), &mut wait)
                    .await
                    .is_err(),
                "a deadline-free client loop produced an event"
            );
            assert!(
                event_tx.try_send(ClientLoopEvent::Quit).is_ok(),
                "client event queue has room for quit"
            );
            wait.await
        };
        let ClientLoopWake::Event(event) = wake else {
            panic!("quit input did not wake the client loop");
        };
        assert!(matches!(&event, ClientLoopEvent::Quit));
        assert!(matches!(
            client_loop
                .handle_event(event, now)
                .expect("quit event is handled"),
            ClientLoopAction::Exit
        ));
    }

    #[test]
    fn client_timer_uses_the_earliest_reported_deadline() {
        let now = Instant::now();
        let shell_deadline = now + Duration::from_secs(3);
        let health_deadline = now + Duration::from_secs(1);
        let retry_deadline = now + Duration::from_secs(2);

        assert_eq!(earliest_client_timer_deadline([None, None, None]), None);
        assert_eq!(
            earliest_client_timer_deadline([
                Some(shell_deadline),
                Some(health_deadline),
                Some(retry_deadline),
            ]),
            Some(health_deadline)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_pending_loop_deadline_is_handled_once() {
        let now = tokio::time::Instant::now().into_std();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let endpoint_id = endpoint::ClientEndpointId::Ssh(
            shepr_config::MachineLabel::parse("timer-test").expect("test machine label"),
        );
        let mut registry = endpoint::EndpointRegistry::empty();
        registry.insert(
            endpoint_id,
            TimerTransport(Arc::clone(&sent)),
            crate::tests::test_generation(1),
            false,
            now,
        );
        let (mut client_loop, event_tx) = test_client_loop(registry);
        let due_in = client_loop
            .next_timer_deadline(now)
            .expect("the SSH endpoint has a health deadline")
            .saturating_duration_since(now);
        assert!(due_in > Duration::from_millis(1));
        // The paused clock jumps straight to the earliest timer, so a loop
        // that armed nothing, or armed a later deadline, runs out the second
        // timeout, and one that armed an earlier deadline wakes inside the
        // first.
        let wake = {
            let wait = client_loop.wait_for_next_event(now);
            tokio::pin!(wait);
            assert!(
                tokio::time::timeout(due_in - Duration::from_millis(1), &mut wait)
                    .await
                    .is_err(),
                "the health deadline fired before its time"
            );
            tokio::time::timeout(Duration::from_millis(2), &mut wait)
                .await
                .expect("the client loop arms its pending deadline")
        };
        assert!(matches!(wake, ClientLoopWake::Deadline));
        let event = ClientLoopEvent::Timer;
        let fired_at = tokio::time::Instant::now().into_std();
        client_loop
            .handle_event(event, fired_at)
            .expect("health deadline handling succeeds");
        {
            let sent = sent.lock().expect("test precondition: lock is healthy");
            assert_eq!(sent.len(), 1);
            assert!(matches!(sent.first(), Some(ClientMessage::HealthPing)));
        }

        let next_deadline = client_loop
            .next_timer_deadline(fired_at)
            .expect("the outstanding health probe has a timeout deadline");
        assert!(next_deadline > fired_at);
        event_tx
            .send(ClientLoopEvent::Resize(
                shepr_core::geometry::HostGeometry::new(
                    shepr_core::geometry::GridSize::clamped(100, 30),
                    shepr_core::geometry::HostCell::Unknown,
                ),
            ))
            .await
            .expect("client event receiver remains open");
        let ClientLoopWake::Event(event) = client_loop.wait_for_next_event(fired_at).await else {
            panic!("resize input did not wake the client loop");
        };
        assert!(matches!(event, ClientLoopEvent::Resize(_)));
        assert_eq!(
            sent.lock()
                .expect("test precondition: lock is healthy")
                .len(),
            1,
            "servicing the deadline sent one health probe"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn no_loop_deadline_does_not_fire_a_timer() {
        let now = tokio::time::Instant::now().into_std();
        let (mut client_loop, event_tx) = test_client_loop(endpoint::EndpointRegistry::empty());
        let wait = client_loop.wait_for_next_event(now);
        tokio::pin!(wait);
        // The paused clock jumps to the earliest timer: any timer the loop
        // armed would wake it before this timeout runs out.
        assert!(
            tokio::time::timeout(Duration::from_secs(60), &mut wait)
                .await
                .is_err(),
            "a deadline-free client loop produced an event"
        );

        event_tx
            .send(ClientLoopEvent::Resize(
                shepr_core::geometry::HostGeometry::new(
                    shepr_core::geometry::GridSize::clamped(100, 30),
                    shepr_core::geometry::HostCell::Unknown,
                ),
            ))
            .await
            .expect("client event receiver remains open");
        let ClientLoopWake::Event(event) = wait.await else {
            panic!("resize input did not wake the client loop");
        };
        assert!(matches!(event, ClientLoopEvent::Resize(_)));
    }

    /// A host terminal that refuses every write while `refusing` is set and
    /// otherwise keeps what it is given.
    #[derive(Clone, Default)]
    struct FlakyHost {
        refusing: Arc<AtomicBool>,
        written: Arc<Mutex<Vec<u8>>>,
    }

    impl FlakyHost {
        fn take_written(&self) -> Vec<u8> {
            std::mem::take(&mut *self.written.lock().expect("test output lock"))
        }
    }

    impl io::Write for FlakyHost {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.refusing.load(Ordering::Relaxed) {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            self.written
                .lock()
                .map_err(|_| io::Error::other("test output lock poisoned"))?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Waits for the loop's next wake with nothing queued and handles it,
    /// asserting the loop armed a timer of its own to wake on.
    async fn wake_on_the_loops_own_timer(client_loop: &mut ClientLoop, now: Instant) {
        let wake = tokio::time::timeout(
            Duration::from_secs(60),
            client_loop.wait_for_next_event(now),
        )
        .await
        .expect("the loop arms a timer to repaint the refused output");
        assert!(matches!(wake, ClientLoopWake::Deadline));
        let fired_at = tokio::time::Instant::now().into_std();
        client_loop
            .handle_event(ClientLoopEvent::Timer, fired_at)
            .expect("the timer is handled");
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_frame_is_repainted_without_waiting_for_another_event() {
        let now = tokio::time::Instant::now().into_std();
        let (mut client_loop, _event_tx) = test_client_loop(endpoint::EndpointRegistry::empty());
        let host = FlakyHost::default();
        client_loop.state.output_writer = Box::new(host.clone());
        host.refusing.store(true, Ordering::Relaxed);
        client_loop.state.mark_chrome_dirty();
        client_loop.state.present_pending();
        assert!(client_loop.state.repaint_pending);

        host.refusing.store(false, Ordering::Relaxed);
        wake_on_the_loops_own_timer(&mut client_loop, now).await;
        assert!(!client_loop.state.repaint_pending);
        assert!(!host.take_written().is_empty(), "the frame was repainted");
        assert_eq!(
            client_loop.next_timer_deadline(tokio::time::Instant::now().into_std()),
            None,
            "a repaint that reached the host arms no further retry"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_surface_patch_is_repainted_without_waiting_for_another_event() {
        let now = tokio::time::Instant::now().into_std();
        let (mut client_loop, _event_tx) = test_client_loop(endpoint::EndpointRegistry::empty());
        let host = FlakyHost::default();
        client_loop.state.output_writer = Box::new(host.clone());
        client_loop.state.mark_chrome_dirty();
        client_loop.state.present_pending();
        assert!(!client_loop.state.repaint_pending, "test precondition");
        host.take_written();

        host.refusing.store(true, Ordering::Relaxed);
        client_loop
            .state
            .queue_surface_patch(crate::shell::ClientComposedSurfacePatch {
                rows: vec![shepr_protocol::PaneSurfacePatchRow {
                    x: 0,
                    y: 0,
                    cells: vec![shepr_protocol::CellData {
                        symbol: "b".into(),
                        ..shepr_protocol::CellData::blank()
                    }],
                }],
                cursor: None,
            });
        client_loop.state.present_pending();
        assert!(client_loop.state.repaint_pending);

        host.refusing.store(false, Ordering::Relaxed);
        wake_on_the_loops_own_timer(&mut client_loop, now).await;
        assert!(!client_loop.state.repaint_pending);
        assert!(!host.take_written().is_empty(), "the frame was repainted");
    }

    /// A host that keeps refusing is retried less and less often, not on
    /// every turn of the loop.
    #[tokio::test(start_paused = true)]
    async fn a_host_that_keeps_refusing_is_retried_with_a_growing_delay() {
        let (mut client_loop, _event_tx) = test_client_loop(endpoint::EndpointRegistry::empty());
        let host = FlakyHost::default();
        client_loop.state.output_writer = Box::new(host.clone());
        host.refusing.store(true, Ordering::Relaxed);
        client_loop.state.mark_chrome_dirty();
        client_loop.state.present_pending();

        let mut delays = Vec::new();
        for _ in 0..4 {
            let now = tokio::time::Instant::now().into_std();
            let due = client_loop
                .next_timer_deadline(now)
                .expect("a refused frame arms a retry");
            delays.push(due.saturating_duration_since(now));
            wake_on_the_loops_own_timer(&mut client_loop, now).await;
            assert!(client_loop.state.repaint_pending);
        }
        assert!(
            delays.windows(2).all(|pair| pair[0] < pair[1]),
            "{delays:?}"
        );
    }
}
