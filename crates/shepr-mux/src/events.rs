//! Internal app events delivered via channel.
//!
//! PTY child watchers, detectors, hook reports and the Git refresh send events
//! to the main loop through this channel. No polling needed.

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
    /// A pane's child process exited.
    PaneDied {
        pane_id: PaneId,
        exit_reason: shepr_platform::ChildExitReason,
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
        source: shepr_agent::agent::AgentSource,
        agent_label: String,
        state: AgentState,
        seq: Option<u64>,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
    },
    /// Agent session identity was reported without state authority.
    AgentSessionReported {
        pane_id: PaneId,
        source: shepr_agent::agent::AgentSource,
        agent_label: String,
        seq: Option<u64>,
        session_ref: Option<shepr_agent::agent::resume::AgentSessionRef>,
        session_start_source: Option<shepr_agent::agent::resume::AgentSessionStartSource>,
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
    origin: Option<(PaneId, RuntimeGeneration)>,
}

impl From<tokio::sync::mpsc::Sender<AppEvent>> for EventSender {
    fn from(sender: tokio::sync::mpsc::Sender<AppEvent>) -> Self {
        Self {
            sender,
            origin: None,
        }
    }
}

impl EventSender {
    pub(crate) fn runtime(
        sender: tokio::sync::mpsc::Sender<AppEvent>,
        pane_id: PaneId,
        generation: RuntimeGeneration,
    ) -> Self {
        Self {
            sender,
            origin: Some((pane_id, generation)),
        }
    }

    fn tag(&self, event: AppEvent) -> AppEvent {
        match self.origin {
            Some((pane_id, generation)) => AppEvent::Runtime {
                pane_id,
                generation,
                event: Box::new(event),
            },
            None => event,
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

    pub(crate) fn blocking_send(
        &self,
        event: AppEvent,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<AppEvent>> {
        self.sender.blocking_send(self.tag(event))
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
        std::thread::spawn(move || {
            sender
                .blocking_send(AppEvent::PaneDied {
                    pane_id,
                    exit_reason: shepr_platform::ChildExitReason::Interrupted,
                })
                .expect("child watcher event");
        })
        .join()
        .expect("producer thread");
        for _ in 0..3 {
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
