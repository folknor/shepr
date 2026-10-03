use crate::errors::LoopExit;
use crate::events::{ClientLoopEvent, ParsedHostInput};
use crate::reconcile::present_notice;
use crate::shell_runtime::{
    ShellInputDisposition, finish_client_shell_input, resize_views,
    settle_expired_endpoint_commands, sync_client_shell_keyboard_report_all, view_geometry,
};
use crate::state::ClientState;
use crate::terminal_geometry::{
    AtomicCellSize, reported_cell_size_from_events, store_reported_cell_size,
};
use crate::{endpoint, fatal_panic, shell, terminal_geometry};
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

pub(crate) struct ClientLoop {
    pub(crate) state: ClientState,
    pub(crate) local_failure_policy: endpoint::LocalFailurePolicy,
    pub(crate) should_quit: Arc<AtomicBool>,
    /// Checked between each step of an iteration and on every way out: once
    /// a panic is latched the loop starts no further step and returns. A step
    /// already under way (a synchronous terminal write) finishes first.
    pub(crate) fatal: Arc<fatal_panic::FatalPanic>,
    // Connections, supervisors, command lanes and view serials serve every endpoint,
    // hidden ones included; the shell's endpoint choice decides only what is shown.
    pub(crate) write_stream: endpoint::EndpointRegistry,
    pub(crate) supervisors: endpoint::EndpointSupervisors,
    pub(crate) endpoint_commands: endpoint::commands::EndpointCommands,
    pub(crate) reported_cell_size: Arc<AtomicCellSize>,
    pub(crate) event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    pub(crate) event_rx: tokio::sync::mpsc::Receiver<ClientLoopEvent>,
    pub(crate) will_query_host_cell_size: bool,
}

fn update_endpoint_status_presentation(
    state: &mut ClientState,
    endpoint_id: &endpoint::ClientEndpointId,
    status: endpoint::EndpointFailureStatus,
    message: &shepr_launch::EndpointFailure,
) {
    state.shell.set_endpoint_status(endpoint_id, status);
    state.shell.set_machine_diagnostic(endpoint_id, message);
    // Handshake diagnostics carry only the failing phase; the status line supplies
    // the configured endpoint label once.
    let unavailable = (status == endpoint::EndpointFailureStatus::Attention
        && state.shell.endpoint_is_active(endpoint_id))
    .then(|| {
        shell::EndpointNotice::new(
            endpoint_id.clone(),
            shell::EndpointNoticeKind::StatusFailure(message.to_string()),
        )
    });
    if let Some(notice) = unavailable {
        present_notice(state, &notice);
    } else {
        state.mark_chrome_dirty();
    }
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
        local_failure_policy: endpoint::LocalFailurePolicy,
        should_quit: Arc<AtomicBool>,
        fatal: Arc<fatal_panic::FatalPanic>,
        write_stream: endpoint::EndpointRegistry,
        supervisors: endpoint::EndpointSupervisors,
        reported_cell_size: Arc<AtomicCellSize>,
        event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
        event_rx: tokio::sync::mpsc::Receiver<ClientLoopEvent>,
        will_query_host_cell_size: bool,
    ) -> Self {
        Self {
            state,
            local_failure_policy,
            should_quit,
            fatal,
            write_stream,
            supervisors,
            endpoint_commands: endpoint::commands::EndpointCommands::default(),
            reported_cell_size,
            event_tx,
            event_rx,
            will_query_host_cell_size,
        }
    }

    fn next_timer_deadline(&mut self, now: std::time::Instant) -> Option<std::time::Instant> {
        earliest_client_timer_deadline([
            self.state.shell.next_timer_deadline(),
            self.state.shell.endpoints.choice.deadline(),
            self.endpoint_commands.next_deadline(),
            self.write_stream.next_service_deadline(now),
            self.supervisors.next_retry_deadline(),
        ])
    }

    /// Returns a quit request at once, else waits for the timer armed from the earliest
    /// pending deadline as of `now` or the shared event queue.
    async fn wait_for_next_event(&mut self, now: std::time::Instant) -> ClientLoopWake {
        let timer_deadline = self.next_timer_deadline(now);
        if self.should_quit.load(Ordering::Acquire) {
            return ClientLoopWake::Event(ClientLoopEvent::Quit);
        }

        tokio::select! {
            biased;
            // Keep wake reasons distinct: a panic and a closed queue are not elapsed timers.
            () = self.fatal.latched() => ClientLoopWake::FatalPanic,
            _ = wait_for_client_timer(timer_deadline) => ClientLoopWake::Deadline,
            ev = self.event_rx.recv() => match ev {
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
        if self.fatal.is_latched() {
            return Err(LoopExit::Panicked);
        }
        result
    }

    async fn run_until_exit(&mut self) -> Result<(), LoopExit> {
        while !self.should_quit.load(Ordering::Acquire) {
            if self.fatal.is_latched() {
                return Err(LoopExit::Panicked);
            }
            // client-clock-sample-ok: the pre-wait sample for supervisors and timers.
            let loop_now = std::time::Instant::now();
            self.reconcile(loop_now)?;
            if self.fatal.is_latched() {
                return Err(LoopExit::Panicked);
            }
            let host_geometry = self.state.reported_geometry;
            let shell = &self.state.shell;
            let mouse_capture = self.state.host_modes.mouse_shell_preference();
            self.supervisors.spawn_due(
                loop_now,
                || endpoint::EndpointConnectOptions {
                    geometry: view_geometry(
                        host_geometry,
                        shell.surface_size(host_geometry.cols(), host_geometry.rows()),
                    ),
                    mouse_capture,
                },
                &self.event_tx,
            );
            let wake = self.wait_for_next_event(loop_now).await;
            if self.fatal.is_latched() {
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
                let mouse = self.state.host_modes.apply_mouse(
                    &mut self.state.output_writer,
                    self.state.reported_geometry.exact(),
                    true,
                );
                self.state
                    .record_host_mode_write("mouse mode retry", mouse)?;
                sync_client_shell_keyboard_report_all(&mut self.state)?;
            }
            self.state.present_pending();
        }
        Ok(action)
    }

    fn handle_stdin_input(
        &mut self,
        inputs: Vec<ParsedHostInput>,
        now: std::time::Instant,
    ) -> Result<ClientLoopAction, LoopExit> {
        let Self {
            state,
            write_stream,
            endpoint_commands,
            reported_cell_size,
            will_query_host_cell_size,
            ..
        } = self;
        let raw_events = inputs.iter().map(|input| &input.event);
        if *will_query_host_cell_size && let Some(cell) = reported_cell_size_from_events(raw_events)
        {
            store_reported_cell_size(reported_cell_size, cell);
        }
        if shepr_termio::input::raw_input::events_require_host_mode_refresh(
            inputs.iter().map(|input| &input.event),
        ) && let Err(error) = state.host_modes.apply_mouse(
            &mut state.output_writer,
            state.reported_geometry.exact(),
            true,
        ) {
            // Reassertion repeats a mode the host already accepted after a host event that
            // may have reset it; a failure here is logged and never ends the session.
            warn!(%error, "failed to re-assert host mouse capture");
        }
        let host_reports_all_keys = state.host_modes.keyboard_report_all_active();
        let shell = &mut state.shell;
        let outcome = shell.handle_host_input(inputs, host_reports_all_keys, now);
        if finish_client_shell_input(state, outcome, write_stream, endpoint_commands, now)?
            == ShellInputDisposition::Detach
        {
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
        let Self {
            state,
            write_stream,
            ..
        } = self;
        let geometry = terminal_geometry::bounded_cell_geometry(geometry);
        state.reported_geometry = geometry;
        let pixel_geometry_exact = geometry.exact();
        let mouse =
            state
                .host_modes
                .apply_mouse(&mut state.output_writer, pixel_geometry_exact, false);
        state.record_host_mode_write("mouse mode resize", mouse)?;
        state.set_host_size(geometry.cols(), geometry.rows());
        // Resizing invalidates the host-side blit baseline. The retained pane surface
        // stays: until the resized one arrives, `compose` draws it clipped to the new
        // pane area (with pane hits clipped to match) instead of dropping to the
        // machine-list placeholder.
        state.request_repaint();
        resize_views(state, write_stream);
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
        let Self {
            supervisors,
            state,
            write_stream,
            ..
        } = self;
        match event {
            endpoint::EndpointSupervisorEvent::Status {
                endpoint_id,
                generation,
                status,
                message,
                connector,
            } => {
                supervisors.return_connector(&endpoint_id, generation, connector);
                if !supervisors.record_status(&endpoint_id, generation, status.into(), now) {
                    return Ok(ClientLoopAction::NextEvent);
                }
                if status == endpoint::EndpointFailureStatus::Attention {
                    warn!(endpoint = %endpoint_id, generation, error = %message, "endpoint needs attention");
                }
                update_endpoint_status_presentation(state, &endpoint_id, status, &message);
            }
            endpoint::EndpointSupervisorEvent::Connected {
                endpoint_id,
                generation,
                connection,
                connector,
            } => {
                supervisors.return_connector(&endpoint_id, generation, connector);
                if !supervisors.record_status(
                    &endpoint_id,
                    generation,
                    endpoint::ClientEndpointStatus::Online,
                    now,
                ) {
                    return Ok(ClientLoopAction::NextEvent);
                }
                write_stream.insert_native(
                    endpoint_id.clone(),
                    connection.activate(),
                    generation,
                    false,
                    now,
                );
                state.shell.endpoint_connected(&endpoint_id, generation);
                // Connecting changes no pane projection (the connection has no
                // surface yet), only the machine list.
                state.mark_chrome_dirty();
            }
        };
        Ok(ClientLoopAction::NextEvent)
    }

    fn handle_server_disconnected(
        &mut self,
        endpoint_id: &endpoint::ClientEndpointId,
        generation: u64,
        error: &io::Error,
    ) -> Result<ClientLoopAction, LoopExit> {
        let Self { write_stream, .. } = self;
        if !write_stream.accepts(endpoint_id, generation) {
            return Ok(ClientLoopAction::NextEvent);
        }
        write_stream.fail(endpoint_id, error);
        Ok(ClientLoopAction::NextEvent)
    }

    fn handle_timer(&mut self, now: std::time::Instant) -> Result<ClientLoopAction, LoopExit> {
        let Self {
            write_stream,
            state,
            endpoint_commands,
            ..
        } = self;
        write_stream.tick_health(now);
        let shell = &mut state.shell;
        let outcome = {
            let mut outcome = shell.tick_timers(now);
            outcome.merge(settle_expired_endpoint_commands(
                endpoint_commands,
                write_stream,
                shell,
                now,
            ));
            outcome
        };
        if finish_client_shell_input(state, outcome, write_stream, endpoint_commands, now)?
            == ShellInputDisposition::Detach
        {
            return Ok(ClientLoopAction::Exit);
        }
        Ok(ClientLoopAction::NextEvent)
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
        now: Instant,
        write_stream: endpoint::EndpointRegistry,
    ) -> (ClientLoop, tokio::sync::mpsc::Sender<ClientLoopEvent>) {
        let supervisors = endpoint::EndpointSupervisors::new(Vec::new(), now)
            .expect("test precondition: no configured supervisors");
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(1);
        (
            ClientLoop::new(
                ClientState::test_new(),
                endpoint::LocalFailurePolicy::Reconnect,
                Arc::new(AtomicBool::new(false)),
                Arc::default(),
                write_stream,
                supervisors,
                Arc::new(AtomicCellSize::new()),
                event_tx.clone(),
                event_rx,
                false,
            ),
            event_tx,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn a_panic_latched_elsewhere_ends_a_waiting_loop() {
        let now = tokio::time::Instant::now().into_std();
        let (mut client_loop, _event_tx) =
            test_client_loop(now, endpoint::EndpointRegistry::empty());
        let fatal = Arc::clone(&client_loop.fatal);
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
        let now = tokio::time::Instant::now().into_std();
        let (mut client_loop, event_tx) =
            test_client_loop(now, endpoint::EndpointRegistry::empty());
        client_loop.fatal.latch();
        event_tx
            .try_send(ClientLoopEvent::Quit)
            .expect("test precondition");
        assert!(matches!(client_loop.run().await, Err(LoopExit::Panicked)));
        assert!(
            client_loop.event_rx.try_recv().is_ok(),
            "the queued event was never handled"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn quit_event_wakes_a_deadline_free_loop() {
        let now = tokio::time::Instant::now().into_std();
        let (mut client_loop, event_tx) =
            test_client_loop(now, endpoint::EndpointRegistry::empty());
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
            1,
            false,
            now,
        );
        let (mut client_loop, event_tx) = test_client_loop(now, registry);
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
                shepr_core::geometry::HostGeometry::new(100, 30, 0, 0, false),
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
        let (mut client_loop, event_tx) =
            test_client_loop(now, endpoint::EndpointRegistry::empty());
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
                shepr_core::geometry::HostGeometry::new(100, 30, 0, 0, false),
            ))
            .await
            .expect("client event receiver remains open");
        let ClientLoopWake::Event(event) = wait.await else {
            panic!("resize input did not wake the client loop");
        };
        assert!(matches!(event, ClientLoopEvent::Resize(_)));
    }
}
