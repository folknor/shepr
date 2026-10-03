//! Internal app events delivered via channel.
//!
//! Pane launch coordinators (each launch's settlement and the pane's death),
//! detectors and the Git refresh send events to the main loop through this
//! channel. No polling needed.

use std::time::Instant;

use shepr_agent::Agent;
use shepr_core::layout::PaneId;

/// An event from a background task to the main loop.
#[derive(Debug)]
pub enum AppEvent {
    /// Runtime events are admitted only while their producing runtime is registered.
    Runtime {
        pane_id: PaneId,
        generation: RuntimeGeneration,
        event: Box<RuntimeEvent>,
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
        detection: shepr_detect::Detection,
        process_exited: bool,
        observed_at: Instant,
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
    /// The Git status worker answered one refresh: a status per workspace it
    /// was asked about (none if the refresh panicked) and the read errors it
    /// saw first. The worker's cache stays on its own thread.
    GitStatusRefreshed {
        outcome: shepr_git::RefreshOutcome<shepr_protocol::WorkspaceId>,
    },
}

/// Only payloads produced by a pane runtime. The sender owns their pane identity.
#[derive(Debug)]
pub enum RuntimeEvent {
    /// A pane's launch settled: its shell's exec committed, it reported why
    /// it could not start, or it ended without a report. Always queued before
    /// the same runtime's `PaneDied`.
    PaneLaunchSettled {
        settlement: crate::pane::LaunchSettlement,
    },
    /// A pane's child process exited. `ended_at` is when the ending was
    /// observed, which can be well before the event is handled.
    PaneDied {
        exit_reason: shepr_platform::ChildExitReason,
        ended_at: Instant,
    },
    /// Process detection identified an agent before its screen state was confirmed.
    AgentProcessDetected { agent: Agent, observed_at: Instant },
    /// Fallback detector state changed in a pane.
    StateChanged {
        agent: Option<Agent>,
        detection: shepr_detect::Detection,
        process_exited: bool,
        observed_at: Instant,
    },
    /// A pane child emitted a valid OSC 52 clipboard write. The main loop
    /// re-emits it to the clients viewing `pane_id`.
    ClipboardWrite { content: Vec<u8> },
    /// A pane child reported its shell current directory through terminal
    /// metadata such as OSC 7.
    TerminalCwdReported { cwd: crate::UsableCwd },
}

impl RuntimeEvent {
    pub fn into_app_event(self, pane_id: PaneId) -> AppEvent {
        match self {
            Self::PaneLaunchSettled { settlement } => AppEvent::PaneLaunchSettled {
                pane_id,
                settlement,
            },
            Self::PaneDied {
                exit_reason,
                ended_at,
            } => AppEvent::PaneDied {
                pane_id,
                exit_reason,
                ended_at,
            },
            Self::AgentProcessDetected { agent, observed_at } => AppEvent::AgentProcessDetected {
                pane_id,
                agent,
                observed_at,
            },
            Self::StateChanged {
                agent,
                detection,
                process_exited,
                observed_at,
            } => AppEvent::StateChanged {
                pane_id,
                agent,
                detection,
                process_exited,
                observed_at,
            },
            Self::ClipboardWrite { content } => AppEvent::ClipboardWrite { pane_id, content },
            Self::TerminalCwdReported { cwd } => AppEvent::TerminalCwdReported { pane_id, cwd },
        }
    }
}

impl TryFrom<AppEvent> for RuntimeEvent {
    type Error = AppEvent;

    fn try_from(event: AppEvent) -> Result<Self, Self::Error> {
        match event {
            AppEvent::PaneLaunchSettled { settlement, .. } => {
                Ok(Self::PaneLaunchSettled { settlement })
            }
            AppEvent::PaneDied {
                exit_reason,
                ended_at,
                ..
            } => Ok(Self::PaneDied {
                exit_reason,
                ended_at,
            }),
            AppEvent::AgentProcessDetected {
                agent, observed_at, ..
            } => Ok(Self::AgentProcessDetected { agent, observed_at }),
            AppEvent::StateChanged {
                agent,
                detection,
                process_exited,
                observed_at,
                ..
            } => Ok(Self::StateChanged {
                agent,
                detection,
                process_exited,
                observed_at,
            }),
            AppEvent::ClipboardWrite { content, .. } => Ok(Self::ClipboardWrite { content }),
            AppEvent::TerminalCwdReported { cwd, .. } => Ok(Self::TerminalCwdReported { cwd }),
            other => Err(other),
        }
    }
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

    /// The pane every payload from this sender belongs to.
    pub(crate) fn pane_id(&self) -> PaneId {
        self.origin.0
    }

    fn tag(&self, event: RuntimeEvent) -> AppEvent {
        let (pane_id, generation) = self.origin;
        AppEvent::Runtime {
            pane_id,
            generation,
            event: Box::new(event),
        }
    }

    pub(crate) async fn send(
        &self,
        event: RuntimeEvent,
    ) -> Result<(), tokio::sync::mpsc::error::SendError<AppEvent>> {
        self.sender.send(self.tag(event)).await
    }

    pub(crate) fn try_send(
        &self,
        event: RuntimeEvent,
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
            .try_send(crate::events::RuntimeEvent::ClipboardWrite {
                content: Vec::new(),
            })
            .expect("nonblocking event");
        sender
            .send(crate::events::RuntimeEvent::AgentProcessDetected {
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
