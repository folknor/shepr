//! The test harness for an `App`: the app together with the outputs the loop
//! would own, so app-level tests can wait on its events and saves.
//!
//! `Deref` makes method calls resolve to `App`; `TestApp` must not define a
//! method whose name `App` also has.

use std::ops::{Deref, DerefMut};

use shepr_mux::events::AppEvent;
use tokio::sync::mpsc;

use super::{App, AppOutputs};

pub(crate) struct TestApp {
    app: App,
    outputs: AppOutputs,
}

impl Deref for TestApp {
    type Target = App;

    fn deref(&self) -> &App {
        &self.app
    }
}

impl DerefMut for TestApp {
    fn deref_mut(&mut self) -> &mut App {
        &mut self.app
    }
}

impl TestApp {
    pub(super) fn new(app: App, outputs: AppOutputs) -> Self {
        Self { app, outputs }
    }

    pub(crate) fn into_parts(self) -> (App, AppOutputs) {
        (self.app, self.outputs)
    }

    pub(crate) async fn next_event(&mut self) -> AppEvent {
        self.outputs.next_event().await
    }

    pub(crate) fn blocking_next_event(&mut self) -> AppEvent {
        self.outputs.blocking_next_event()
    }

    pub(crate) fn event_sender(&self) -> mpsc::Sender<AppEvent> {
        self.outputs.event_sender()
    }

    /// Whether no event is queued.
    pub(crate) fn no_queued_events(&self) -> bool {
        self.outputs.no_queued_events()
    }

    /// Turns the test app into a persisting one, as production boots: a
    /// threaded persister on the same data directory, fired through this
    /// harness's `save_finished`. Tests set up their state first, so that
    /// setup schedules no saves.
    pub(crate) fn persist(&mut self) {
        let save_finished = self.outputs.save_finished_signal();
        self.app.persist_with_signal(save_finished);
    }
}
