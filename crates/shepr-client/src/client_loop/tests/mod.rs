//! A `ClientLoop` showing Local with the machine `build` connected beside it, driven the way
//! launch, the endpoint readers and host input drive it: reconcile turns, server messages,
//! host events and shell output.

use super::{ClientLoop, EventQueue, HostCellReport, LoopSignals};
use crate::endpoint::{self, ClientEndpointId, EndpointRegistry};
use crate::events::ClientLoopEvent;
use crate::shell;
use crate::shell_runtime::finish_client_shell_input;
use crate::state::ClientState;
use crate::terminal_geometry::AtomicCellSize;
use crate::tests::endpoints::{RecordingTransport, boot, remote, snapshot, surface};
use crate::tests::test_generation;
use shepr_protocol::{
    ClientMessage, ClientSurfaceSize, ServerMessage,
    command::{EndpointCommand, EndpointReply},
};
use shepr_surface::decode::DecodedClientServerMessage;
use shepr_test_fixtures::ValidatedClientConfigFixture as _;
use shepr_test_fixtures::counter_at;
use std::io;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

mod endpoint_move;
mod surface_baseline;

/// The host terminal: keeps everything the loop writes.
#[derive(Clone, Default)]
struct Output(Arc<Mutex<Vec<u8>>>);

impl io::Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("output lock"))?
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) struct Fixture {
    pub(super) client: ClientLoop,
    pub(super) local: RecordingTransport,
    pub(super) target: RecordingTransport,
    pub(super) now: Instant,
    output: Output,
}

impl Fixture {
    pub(super) fn new() -> Self {
        let now = Instant::now();
        let config = shepr_config::ValidatedClientConfig::test_default();
        let machines = vec![shepr_config::MachineConfig {
            label: shepr_config::MachineLabel::parse("build").expect("machine"),
            ssh: shepr_config::SshTarget::parse("host").expect("SSH"),
            palette: shepr_config::DEFAULT_LOCAL_HUE,
        }];
        // `test_new` reports a 100x30 host, the size `surface_size` is asked for below.
        let mut state = ClientState::test_new();
        state.shell.set_machines(&machines);
        let output = Output::default();
        state.output_writer = Box::new(output.clone());
        state
            .shell
            .endpoint_connected(&ClientEndpointId::Local, test_generation(1));
        state.shell.set_endpoint_snapshot_for_generation(
            &ClientEndpointId::Local,
            test_generation(1),
            snapshot(&ClientEndpointId::Local, 1),
        );
        state
            .shell
            .endpoint_connected(&remote(), test_generation(7));
        state.shell.cache_endpoint_snapshot_for_generation(
            &remote(),
            test_generation(7),
            snapshot(&remote(), 1),
        );
        let size = state.shell.surface_size(100, 30);
        state.shell.receive_pane_surface_from(
            surface(&ClientEndpointId::Local, 1, size, "SOURCE"),
            test_generation(1),
        );
        let local = RecordingTransport::default();
        let target = RecordingTransport::default();
        let mut registry =
            EndpointRegistry::new_synthetic_at(local.clone(), test_generation(1), now);
        registry.insert(remote(), target.clone(), test_generation(7), false, now);
        let supervisors = endpoint::EndpointSupervisors::new(
            endpoint::EndpointSupervisors::fresh_connectors(config.paths(), &machines),
            now,
        )
        .expect("supervisors");
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let hub = endpoint::EndpointHub::new(
            registry,
            supervisors,
            endpoint::LocalFailurePolicy::Reconnect,
        );
        let client = ClientLoop::new(
            state,
            hub,
            LoopSignals {
                should_quit: Arc::new(AtomicBool::new(false)),
                fatal: Arc::default(),
            },
            EventQueue { tx, rx },
            HostCellReport {
                size: Arc::new(AtomicCellSize::new()),
                queried: crate::input::ProbeAvailability::NotArmed,
            },
        );
        Self {
            client,
            local,
            target,
            now,
            output,
        }
    }

    pub(super) fn size(&self) -> ClientSurfaceSize {
        self.client.state().shell.surface_size(
            self.client.state().reported_geometry.cols(),
            self.client.state().reported_geometry.rows(),
        )
    }

    /// A shell pick of `destination`, carried out by the hub as the shell's output.
    pub(super) fn select(&mut self, destination: shell::Location) {
        let (state, hub) = self.client.parts_mut();
        hub.dispatch(
            &mut state.shell,
            vec![shell::ClientShellAction::ActivateEndpoint(destination)],
            self.now,
        );
    }

    /// A shell pick of the machine `id`, with no navigation.
    pub(super) fn pick(&mut self, id: ClientEndpointId) {
        self.select(shell::Location::machine(id));
    }

    pub(super) fn reconcile(&mut self) {
        self.client.reconcile(self.now).expect("reconcile");
    }

    /// Picks the machine and runs the reconcile turn that turns it on.
    pub(super) fn start(&mut self) {
        self.pick(remote());
        self.reconcile();
    }

    /// Loses Local's connection and runs the reconcile turn that handles it: nothing is
    /// shown, and the choice waits for Local's next connection.
    pub(super) fn lose_local(&mut self) {
        self.client.hub_mut().registry_mut().fail(
            &ClientEndpointId::Local,
            &io::Error::new(io::ErrorKind::BrokenPipe, "lost"),
        );
        self.reconcile();
    }

    pub(super) fn inbound(&mut self, id: &ClientEndpointId, message: ServerMessage) {
        let message = shepr_surface::decode::Decoder::default()
            .decode_client(message)
            .expect("test message is valid");
        let generation = self
            .client
            .hub()
            .registry()
            .connection(id)
            .expect("connection")
            .generation;
        self.client
            .handle_event(
                ClientLoopEvent::ServerMessage {
                    endpoint_id: id.clone(),
                    generation,
                    message: Box::new(message),
                },
                self.now,
            )
            .expect("message");
    }

    pub(super) fn inbound_patch(
        &mut self,
        id: &ClientEndpointId,
        patch: shepr_protocol::PaneSurfacePatch,
    ) {
        let generation = self
            .client
            .hub()
            .registry()
            .connection(id)
            .expect("connection")
            .generation;
        self.client
            .handle_event(
                ClientLoopEvent::ServerMessage {
                    endpoint_id: id.clone(),
                    generation,
                    message: Box::new(DecodedClientServerMessage::PaneSurfacePatch(patch)),
                },
                self.now,
            )
            .expect("patch");
    }

    /// The last view-on request the machine was sent.
    pub(super) fn on_request(&self) -> shepr_protocol::RequestId {
        self.target
            .sent
            .lock()
            .expect("messages")
            .iter()
            .rev()
            .find_map(|m| match m {
                ClientMessage::ClientShellEndpointRequest {
                    request_id,
                    command: EndpointCommand::ClientShellSurfaceSet(p),
                    ..
                } if p.active => Some(request_id.clone()),
                _ => None,
            })
            .expect("on request")
    }

    /// The machine acknowledges the last view-on request and sends the snapshot and surface
    /// of the acknowledged projection.
    pub(super) fn evidence(&mut self) {
        self.evidence_for(self.on_request());
    }

    pub(super) fn evidence_for(&mut self, request_id: shepr_protocol::RequestId) {
        self.inbound(
            &remote(),
            ServerMessage::ClientShellEndpointResponse {
                boot_id: boot(&remote()),
                request_id,
                result: Ok(EndpointReply::ClientShellSurfaceSet {
                    active: true,
                    projection_revision: counter_at(2),
                }),
            },
        );
        self.inbound(
            &remote(),
            ServerMessage::EndpointSnapshot(snapshot(&remote(), 2)),
        );
        self.inbound(
            &remote(),
            ServerMessage::PaneSurface(surface(&remote(), 2, self.size(), "TARGET")),
        );
    }

    pub(super) fn commit(&mut self) {
        self.evidence();
        self.reconcile();
    }

    pub(super) fn output(&self) -> String {
        String::from_utf8_lossy(&self.output.0.lock().expect("output")).into_owned()
    }

    pub(super) fn clear_output(&self) {
        self.output.0.lock().expect("output").clear();
    }

    /// Shell output carrying `message`.
    pub(super) fn input(&mut self, message: ClientMessage) {
        // The shell routes host theme updates to every viewed endpoint and
        // everything else to the shown one; the fixture keeps that routing.
        let request = match message {
            ClientMessage::ClientShellHostTheme { update } => {
                shell::ClientShellRequest::HostTheme(update)
            }
            message => shell::ClientShellRequest::Shown(message),
        };
        let (state, hub) = self.client.parts_mut();
        finish_client_shell_input(
            state,
            shell::ClientShellInput {
                requests: vec![request],
                ..Default::default()
            },
            hub,
            self.now,
        )
        .expect("input");
    }

    /// Whatever is shown is viewed, nothing the choice does not want is viewed, and the
    /// shell projects what is shown.
    pub(super) fn assert_views(&self) {
        let choice = self.client.state().shell.endpoints.choice();
        let registry = self.client.hub().registry();
        if let Some(shown) = choice.live()
            && registry.connection(shown).is_some()
        {
            assert!(registry.viewed(shown));
        }
        for id in [ClientEndpointId::Local, remote()] {
            if !choice.wants_view(&id) {
                assert!(!registry.viewed(&id));
            }
        }
        if let Some(shown) = choice.live() {
            assert!(self.client.state().shell.endpoint_is_active(shown));
        }
    }
}
