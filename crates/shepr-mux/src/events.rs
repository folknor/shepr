//! Internal app events delivered via channel.
//!
//! Pane launch coordinators (each launch's settlement and the pane's death),
//! detectors, hook reports and the Git refresh send events to the main loop
//! through this channel. No polling needed.

use std::time::Instant;

use crate::git::{GitStatusCacheEntry, WorkspaceGitStatus};
use shepr_agent::detect::{Agent, AgentState};
use shepr_core::layout::PaneId;

/// An event from a background task to the main loop.
#[derive(Debug)]
pub enum AppEvent {
    /// Runtime events are admitted only while their producing runtime is registered.
    Runtime {
        pane_id: PaneId,
        generation: RuntimeGeneration,
        event: Box<AppEvent>,
    },
    /// A pane's launch settled: its shell's exec committed, it reported why
    /// it could not start, or it ended without a report. Always queued before
    /// the same runtime's `PaneDied`.
    PaneLaunchSettled {
        pane_id: PaneId,
        settlement: crate::pane::LaunchSettlement,
    },
    /// A pane's child process exited. `ended_at` is when the ending was
    /// observed, which can be well before the event is handled.
    PaneDied {
        pane_id: PaneId,
        exit_reason: shepr_platform::ChildExitReason,
        ended_at: Instant,
    },
    /// Process detection identified an agent before its screen state was confirmed.
    AgentProcessDetected {
        pane_id: PaneId,
        agent: Agent,
        observed_at: Instant,
    },
    /// Fallback detector state changed in a pane.
    StateChanged {
        pane_id: PaneId,
        agent: Option<Agent>,
        state: AgentState,
        visible_blocker: bool,
        process_exited: bool,
        observed_at: Instant,
    },
    /// Hook-authoritative agent state was reported for a pane.
    HookStateReported {
        pane_id: PaneId,
        origin: shepr_agent::agent::ReportOrigin,
        state: AgentState,
        seq: Option<u64>,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
    },
    /// Agent session identity was reported without state authority.
    AgentSessionReported {
        pane_id: PaneId,
        origin: shepr_agent::agent::ReportOrigin,
        seq: Option<u64>,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        session_start_source: shepr_agent::agent::resume::ReportedSessionStart,
    },
    /// A pane child emitted a valid OSC 52 clipboard write. The main loop
    /// re-emits it to the clients viewing `pane_id`.
    ClipboardWrite { pane_id: PaneId, content: Vec<u8> },
    /// A pane child reported its shell current directory through terminal
    /// metadata such as OSC 7.
    TerminalCwdReported {
        pane_id: PaneId,
        cwd: crate::UsableCwd,
    },
    /// Background git status refresh completed for workspaces.
    GitStatusRefreshed {
        results: Vec<WorkspaceGitStatus>,
        cache_updates: Vec<(std::path::PathBuf, GitStatusCacheEntry)>,
    },
}

/// Process-local identity of one runtime, independent of its durable pane id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeGeneration(u64);

impl RuntimeGeneration {
    pub fn alloc() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }
}

/// Tags every runtime-originated event at its producer boundary.
#[derive(Clone)]
pub(crate) struct EventSender {
    sender: tokio::sync::mpsc::Sender<AppEvent>,
    origin: (PaneId, RuntimeGeneration),
}

impl EventSender {
    pub(crate) fn runtime(
        sender: tokio::sync::mpsc::Sender<AppEvent>,
        pane_id: PaneId,
        generation: RuntimeGeneration,
    ) -> Self {
        Self {
            sender,
            origin: (pane_id, generation),
        }
    }

    fn tag(&self, event: AppEvent) -> AppEvent {
        let (pane_id, generation) = self.origin;
        AppEvent::Runtime {
            pane_id,
            generation,
            event: Box::new(event),
        }
    }

    pub(crate) async fn send(
        &self,
        event: AppEvent,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<AppEvent>> {
        self.sender.send(self.tag(event)).await
    }

    pub(crate) fn try_send(
        &self,
        event: AppEvent,
    ) -> Result<(), tokio::sync::mpsc::error::TrySendError<AppEvent>> {
        self.sender.try_send(self.tag(event))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runtime_sender_tags_every_delivery_mode_with_one_generation() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let pane_id = PaneId::alloc();
        let generation = RuntimeGeneration::alloc();
        let sender = EventSender::runtime(tx, pane_id, generation);
        sender
            .try_send(AppEvent::ClipboardWrite {
                pane_id,
                content: Vec::new(),
            })
            .expect("nonblocking event");
        sender
            .send(AppEvent::AgentProcessDetected {
                pane_id,
                agent: Agent::Codex,
                observed_at: Instant::now(),
            })
            .await
            .expect("async event");
        for _ in 0..2 {
            match rx.try_recv().expect("tagged event") {
                AppEvent::Runtime {
                    pane_id: reported_pane,
                    generation: reported_generation,
                    ..
                } => {
                    assert_eq!(reported_pane, pane_id);
                    assert_eq!(reported_generation, generation);
                }
                other => panic!("runtime event escaped its producer boundary: {other:?}"),
            }
        }
    }
}
