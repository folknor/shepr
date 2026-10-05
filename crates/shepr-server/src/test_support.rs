//! Fixtures for this crate's unit tests, built on the public API (and the
//! seams) of the crates they stand in for. Fixtures several crates share live
//! in `shepr-test-fixtures`; these are the ones only this crate's tests use.
//! The mux fixture traits stay here rather than moving there: mux
//! dev-depends on `shepr-test-fixtures`, so that crate cannot depend on mux.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use shepr_agent::{Agent, AgentState};
use shepr_core::layout::{Direction, PaneId};
use shepr_mux::pane::{PaneRuntime, PaneRuntimeRegistry};
use shepr_mux::terminal::{EffectiveStateChange, TerminalState};
use shepr_mux::workspace::{PaneRecord, Workspace};
use tokio::sync::{Notify, mpsc};

pub(crate) use shepr_test_fixtures::{AppPathsFixture, ValidatedServerConfigFixture};
pub(crate) use shepr_test_support::{IsolatedEnv, ScratchDir};

/// Pane runtimes with no child: what the pane writes to its child arrives on
/// the returned receiver (`shepr_test_fixtures::ChannelChildIo`).
pub(crate) trait PaneRuntimeFixture: Sized {
    fn test_with_screen_bytes(cols: u16, rows: u16, bytes: &[u8]) -> Self;
    fn test_with_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
    ) -> Self;
    fn test_with_channel_and_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
        channel_capacity: usize,
    ) -> (Self, mpsc::Receiver<Bytes>);
    /// Feed `bytes` directly to the terminal parser and advance content
    /// revisions. This does not exercise PTY-reader effect dispatch.
    fn test_process_pty_bytes(&self, bytes: &[u8]);
    /// Arrange for a writer to try to land `bytes` while the next dirty-patch
    /// collection holds the terminal core. Once the collection has started,
    /// the writer tries to take the terminal core without waiting, reports
    /// through the handle whether it got it (which would let a write land
    /// mid-collection), then waits for the returned sender before it writes,
    /// blocking on the core if the first attempt failed.
    fn test_contend_during_dirty_collection(
        &self,
        bytes: Vec<u8>,
    ) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<bool>);
    /// Breaks the terminal core the way a parser panic does: another thread
    /// panics while it holds the core, poisoning it.
    fn test_break_terminal_core(&self);
    /// `grid_size()` as `(rows, cols)`, so geometry tests compare it with a
    /// tuple literal. It reads `grid_size()` and keeps no size of its own;
    /// note the order is the reverse of `GridSize::clamped(cols, rows)`.
    fn current_size(&self) -> (u16, u16);
}

impl PaneRuntimeFixture for PaneRuntime {
    fn test_with_screen_bytes(cols: u16, rows: u16, bytes: &[u8]) -> Self {
        Self::test_with_scrollback_bytes(cols, rows, 0, bytes)
    }

    fn test_with_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
    ) -> Self {
        Self::test_with_channel_and_scrollback_bytes(cols, rows, scrollback_limit_bytes, bytes, 4).0
    }

    fn test_with_channel_and_scrollback_bytes(
        cols: u16,
        rows: u16,
        scrollback_limit_bytes: usize,
        bytes: &[u8],
        channel_capacity: usize,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        let (io, rx) = shepr_test_fixtures::ChannelChildIo::new(channel_capacity);
        (
            Self::with_child_io(
                shepr_core::geometry::PaneGeometry::cells_only(cols, rows),
                shepr_core::scrollback::ScrollbackBudget::new(scrollback_limit_bytes),
                bytes,
                Box::new(io),
                Arc::new(Notify::new()),
            ),
            rx,
        )
    }

    fn test_process_pty_bytes(&self, bytes: &[u8]) {
        self.output_writer().begin().write(bytes);
    }

    fn test_contend_during_dirty_collection(
        &self,
        bytes: Vec<u8>,
    ) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<bool>) {
        let writer = self.output_writer();
        let (start_tx, start_rx) = std::sync::mpsc::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        self.on_next_dirty_collection(Box::new(move || {
            start_tx.send(()).expect("test start channel is open");
            ready_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("test ready signal arrives within timeout");
        }));
        let handle = std::thread::spawn(move || {
            start_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("test start signal arrives within timeout");
            let early = writer.try_begin();
            let took_core = early.is_some();
            ready_tx.send(()).expect("test ready channel is open");
            release_rx.recv().expect("test releases the waiting writer");
            early.unwrap_or_else(|| writer.begin()).write(&bytes);
            took_core
        });
        (release_tx, handle)
    }

    fn test_break_terminal_core(&self) {
        let writer = self.output_writer();
        let outcome = std::thread::spawn(move || {
            let _core = writer.begin();
            panic!("break the terminal core for a test");
        })
        .join();
        assert!(outcome.is_err(), "the core holder panicked");
        assert!(self.terminal_core_broken(), "the panic broke the core");
    }

    fn current_size(&self) -> (u16, u16) {
        let grid = self.grid_size();
        (grid.rows.get(), grid.cols.get())
    }
}

pub(crate) trait PaneRuntimeRegistryFixture {
    /// Take every runtime out of the registry.
    fn drain(&mut self) -> std::collections::hash_map::IntoIter<PaneId, PaneRuntime>;
}

impl PaneRuntimeRegistryFixture for PaneRuntimeRegistry {
    fn drain(&mut self) -> std::collections::hash_map::IntoIter<PaneId, PaneRuntime> {
        std::mem::take(self).into_iter()
    }
}

/// Workspaces built without spawning a pane.
pub(crate) trait WorkspaceFixture: Sized {
    /// One pane, named `name`, rooted at `/`: a directory that
    /// exists on every host, so tests that launch the pane can, and that is
    /// neither the runner's cwd nor a git repository.
    fn test_new(name: &str) -> Self;
    /// One pane whose terminal reports `cwd`, which is also the workspace's
    /// identity cwd; `label` is the workspace's name, or `None` to name it
    /// after `cwd`.
    fn test_at(label: Option<&str>, cwd: &Path) -> Self;
    /// Split the focused pane; returns the new pane.
    fn test_split(&mut self, direction: Direction) -> PaneId;
    /// A workspace whose raw pane ids and public pane numbers differ, so code
    /// that confuses them is caught.
    fn test_adversarial_identity_state() -> Self;
    /// Removes a pane that is not the workspace's last; its record.
    fn close_pane(&mut self, pane_id: PaneId) -> Option<PaneRecord>;
}

/// The allocator this crate's fixture workspaces share, so every fixture in
/// the test binary has its own ID, as workspaces of one session do. A state's
/// workspace set that takes a fixture moves its own allocator past the
/// fixture's ID (`AppState::test_push_workspace`, `test_set_workspaces`).
pub(crate) fn next_fixture_workspace_id() -> shepr_protocol::WorkspaceId {
    static TEST_WORKSPACE_IDS: std::sync::Mutex<shepr_mux::workspace::WorkspaceIdAllocator> =
        std::sync::Mutex::new(shepr_mux::workspace::WorkspaceIdAllocator::new());
    TEST_WORKSPACE_IDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .try_allocate()
        .expect("test workspace ID space available")
}

impl WorkspaceFixture for Workspace {
    fn test_new(name: &str) -> Self {
        Self::test_at(Some(name), Path::new("/"))
    }

    fn test_at(label: Option<&str>, cwd: &Path) -> Self {
        let cwd = shepr_core::absolute_path::AbsolutePath::new(cwd).expect("test cwd is absolute");
        Self::test_from_pane(
            next_fixture_workspace_id(),
            label.map(str::to_owned),
            &cwd,
            PaneId::alloc(),
            TerminalState::new(cwd.clone()),
        )
    }

    fn test_split(&mut self, direction: Direction) -> PaneId {
        let chrome = shepr_mux::workspace::WorkspaceChrome {
            area: shepr_core::geometry::Rect::new(0, 0, 80, 24),
            pane_gaps: false,
            pane_scrollbars: false,
        };
        let prepared = self
            .prepare_split(
                self.tree().focused(),
                direction,
                &chrome,
                None,
                shepr_core::absolute_path::AbsolutePath::root(),
            )
            .expect("test split prepares");
        self.commit_split(prepared).expect("test split commits")
    }

    fn test_adversarial_identity_state() -> Self {
        let mut ws = Self::test_new("adversarial-identity");
        let removed_pane = ws.test_split(Direction::Horizontal);
        ws.test_split(Direction::Vertical);
        assert!(ws.close_pane(removed_pane).is_some());
        let _unused_raw_id = PaneId::alloc();
        let later_pane = ws.test_split(Direction::Horizontal);

        assert_ne!(
            later_pane.raw() as usize,
            ws.tree()
                .pane(later_pane)
                .expect("test pane has a record")
                .number()
                .get(),
            "adversarial pane must distinguish raw pane id from public pane number"
        );
        ws
    }

    fn close_pane(&mut self, pane_id: PaneId) -> Option<PaneRecord> {
        self.remove_pane(pane_id).ok()
    }
}

pub(crate) trait TerminalStateFixture {
    /// Screen detection reporting `agent` in `fallback_state` now, with no
    /// visible blocker and the process running.
    fn set_detected_state(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
    ) -> Option<EffectiveStateChange>;
}

impl TerminalStateFixture for TerminalState {
    fn set_detected_state(
        &mut self,
        agent: Option<Agent>,
        fallback_state: AgentState,
    ) -> Option<EffectiveStateChange> {
        self.ownership_mut()
            .set_detected_state_with_screen_signals_at(
                agent,
                fallback_state,
                false,
                false,
                Instant::now(),
            )
            .effective_state_change
    }
}

/// A canonical workspace ID that no live test workspace holds, for public
/// pane IDs a pane kept from a workspace it has left. It spells `usize::MAX`,
/// the number a `WorkspaceIdAllocator` keeps as its exhaustion mark and never
/// hands out, so no allocator issues it to a workspace.
pub(crate) fn retired_workspace_id() -> shepr_protocol::WorkspaceId {
    shepr_protocol::WorkspaceId::from_number(usize::MAX).expect("nonzero public number")
}

/// A workspace ID from its canonical spelling (`w<number>`).
pub(crate) fn test_workspace_id(id: &str) -> shepr_protocol::WorkspaceId {
    id.parse()
        .unwrap_or_else(|_| panic!("{id:?} is not a canonical workspace id"))
}

/// A Codex resume plan for the session named by the last NUL-separated field
/// of `identity`, launching `argv` instead of the real resume command.
pub(crate) fn test_codex_plan(
    identity: &str,
    argv: Vec<String>,
) -> shepr_agent::resume::AgentResumePlan {
    use shepr_agent::resume::{AgentSessionRef, PersistedAgentSession};
    use shepr_agent::{AgentSource, IntegrationTarget};
    let session_id = identity.rsplit('\0').next().unwrap_or(identity);
    let session = PersistedAgentSession::new(
        AgentSource::new(IntegrationTarget::Codex),
        Agent::Codex,
        AgentSessionRef::id(session_id).expect("test session id is valid"),
    )
    .expect("test session is a Codex session");
    let mut argv = argv.into_iter();
    let program = argv.next().expect("test resume command has an executable");
    shepr_agent::resume::AgentResumePlan::for_command(&session, program, argv.collect())
        .expect("test resume command has a nonempty executable")
}

/// An API reply as the JSON a client would read.
pub(crate) trait TestResponseJson {
    fn test_json(&self) -> String;
}

impl TestResponseJson for String {
    fn test_json(&self) -> String {
        self.clone()
    }
}

impl TestResponseJson for shepr_api::error::ApiResult {
    fn test_json(&self) -> String {
        shepr_api::error::encode_result("test".into(), self.clone())
    }
}

pub(crate) fn test_json(response: &impl TestResponseJson) -> String {
    response.test_json()
}
