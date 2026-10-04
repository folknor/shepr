//! What the app and its pane runtimes publish, owned by whoever runs the loop.
//!
//! `App::open` creates the event channel, the render signal and the two wake
//! notifications, hands the sending sides to the pane launcher, the Git worker
//! and the session persister, and returns the receiving sides here. The app
//! keeps none of them: the loop is the only thing that waits on them.

use std::sync::Arc;

use shepr_mux::events::AppEvent;
use shepr_mux::render_signal::RenderSignal;
use tokio::sync::{Notify, mpsc};

/// Why the loop was woken by the app's outputs.
pub(crate) enum AppWake {
    Event(AppEvent),
    /// A pane runtime asked for a render, or a session save ended.
    Signal,
}

pub(crate) struct AppOutputs {
    events: mpsc::Receiver<AppEvent>,
    render: Arc<RenderSignal>,
    render_wake: Arc<Notify>,
    save_finished: Arc<Notify>,
    /// A sender into the event channel, as the pane runtimes hold. Production
    /// senders live with the runtimes; tests take one from here.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "only tests take a sender from the outputs")
    )]
    event_sender: mpsc::Sender<AppEvent>,
}

impl AppOutputs {
    pub(super) fn new(
        events: mpsc::Receiver<AppEvent>,
        render: Arc<RenderSignal>,
        render_wake: Arc<Notify>,
        save_finished: Arc<Notify>,
        event_sender: mpsc::Sender<AppEvent>,
    ) -> Self {
        Self {
            events,
            render,
            render_wake,
            save_finished,
            event_sender,
        }
    }

    /// Waits for an app event, a render request or a finished save. A firing
    /// signal with nothing to do is harmless: the loop's pass reconsiders.
    ///
    /// Cancel safe: mpsc `recv` and `Notify::notified` both are.
    pub(crate) async fn next(&mut self) -> AppWake {
        tokio::select! {
            // The app's own senders keep the channel open while the loop
            // runs, so a closed channel only reads as a wake.
            maybe_ev = self.events.recv() => match maybe_ev {
                Some(ev) => AppWake::Event(ev),
                None => AppWake::Signal,
            },
            () = self.save_finished.notified() => AppWake::Signal,
            () = self.render_wake.notified() => AppWake::Signal,
        }
    }

    /// The next queued event, without waiting.
    pub(crate) fn try_next_event(&mut self) -> Option<AppEvent> {
        self.events.try_recv().ok()
    }

    /// How many events are queued.
    pub(crate) fn queued_events(&self) -> usize {
        self.events.len()
    }

    /// The render requests pane runtimes have made.
    pub(crate) fn render(&self) -> &RenderSignal {
        &self.render
    }
}

#[cfg(test)]
impl AppOutputs {
    /// A sender into the event channel, as the pane runtimes hold.
    pub(crate) fn event_sender(&self) -> mpsc::Sender<AppEvent> {
        self.event_sender.clone()
    }

    /// The signal a session persister built for this app fires when a save
    /// ends.
    pub(crate) fn save_finished_signal(&self) -> Arc<Notify> {
        Arc::clone(&self.save_finished)
    }

    /// Waits for the next event; panics when the channel closed.
    pub(crate) async fn next_event(&mut self) -> AppEvent {
        self.events
            .recv()
            .await
            .expect("the app event channel is open")
    }

    /// Blocks for the next event; panics when the channel closed.
    pub(crate) fn blocking_next_event(&mut self) -> AppEvent {
        self.events
            .blocking_recv()
            .expect("the app event channel is open")
    }

    /// Whether no event is queued.
    pub(crate) fn no_queued_events(&self) -> bool {
        self.events.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn outputs() -> AppOutputs {
        let (event_sender, events) = mpsc::channel(4);
        AppOutputs::new(
            events,
            Arc::new(RenderSignal::new()),
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
            event_sender,
        )
    }

    async fn resolves(outputs: &mut AppOutputs) -> AppWake {
        tokio::time::timeout(Duration::from_secs(5), outputs.next())
            .await
            .expect("the outputs wake")
    }

    #[tokio::test]
    async fn app_outputs_wake_for_events_renders_and_saves() {
        let mut outputs = outputs();

        outputs
            .event_sender()
            .send(AppEvent::GitStatusRefreshed {
                outcome: shepr_git::RefreshOutcome::empty(),
            })
            .await
            .expect("send");
        assert!(matches!(resolves(&mut outputs).await, AppWake::Event(_)));

        outputs.render_wake.notify_one();
        assert!(matches!(resolves(&mut outputs).await, AppWake::Signal));

        outputs.save_finished.notify_one();
        assert!(matches!(resolves(&mut outputs).await, AppWake::Signal));
    }
}
