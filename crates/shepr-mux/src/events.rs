//! Internal app events delivered via channel.
//!
//! Pane launch coordinators (each launch's settlement and the pane's death),
//! detectors and the Git refresh send events to the main loop through this
//! channel. No polling needed.

use std::time::Instant;

use shepr_agent::Agent;
use shepr_core::layout::PaneId;

/// A message from a background task to the main loop. This is transport only:
/// the server admits it (a runtime's generation is checked) into its own
/// reducer input before anything is applied, and no pane event travels bare.
#[derive(Debug)]
pub enum AppEvent {
    /// Runtime events are admitted only while their producing runtime is registered.
    Runtime {
        pane_id: PaneId,
        generation: RuntimeGeneration,
        event: Box<RuntimeEvent>,
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
    /// A pane ended. `ended_at` is when the ending was observed, which can be
    /// well before the event is handled.
    PaneDied {
        ending: crate::pane::PaneEnding,
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
    /// This payload in the envelope that names its producing runtime, which is
    /// the only way a runtime event reaches the main loop.
    pub fn enveloped(self, pane_id: PaneId, generation: RuntimeGeneration) -> AppEvent {
        AppEvent::Runtime {
            pane_id,
            generation,
            event: Box::new(self),
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
        event.enveloped(pane_id, generation)
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
