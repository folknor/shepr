use std::collections::HashMap;

use super::PaneRuntime;
use shepr_protocol::TerminalId;

/// Server-owned live terminal runtimes, keyed by durable terminal id.
///
/// This sits outside `AppState` so pure state can stay focused on workspace,
/// pane, and terminal metadata while the server/application layer owns PTYs,
/// parser backends, detector tasks, and channels.
#[derive(Default)]
pub struct PaneRuntimeRegistry {
    runtimes: HashMap<TerminalId, PaneRuntime>,
}

impl PaneRuntimeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, terminal_id: &TerminalId) -> Option<&PaneRuntime> {
        self.runtimes.get(terminal_id)
    }

    pub fn insert(&mut self, terminal_id: TerminalId, runtime: PaneRuntime) -> Option<PaneRuntime> {
        self.runtimes.insert(terminal_id, runtime)
    }

    pub fn remove(&mut self, terminal_id: &TerminalId) -> Option<PaneRuntime> {
        self.runtimes.remove(terminal_id)
    }

    pub fn clear(&mut self) {
        self.runtimes.clear();
    }

    pub fn values(&self) -> impl Iterator<Item = &PaneRuntime> {
        self.runtimes.values()
    }

    #[cfg(any(test, feature = "test-api"))]
    pub fn drain(&mut self) -> impl Iterator<Item = (TerminalId, PaneRuntime)> + '_ {
        self.runtimes.drain()
    }
}

impl From<HashMap<TerminalId, PaneRuntime>> for PaneRuntimeRegistry {
    fn from(runtimes: HashMap<TerminalId, PaneRuntime>) -> Self {
        Self { runtimes }
    }
}
