use std::collections::HashMap;

use super::PaneRuntime;
use shepr_core::layout::PaneId;

/// Server-owned live pane runtimes, keyed by the pane they run.
///
/// A runtime exists only for a live pane: removing a pane removes and shuts
/// down its runtime, and an agent resume replaces the runtime under the same
/// `PaneId`.
///
/// This sits outside `AppState` so pure state can stay focused on workspace,
/// pane, and terminal metadata while the server/application layer owns PTYs,
/// parser backends, detector tasks, and channels.
#[derive(Default)]
pub struct PaneRuntimeRegistry {
    runtimes: HashMap<PaneId, PaneRuntime>,
}

impl PaneRuntimeRegistry {
    pub fn get(&self, pane: &PaneId) -> Option<&PaneRuntime> {
        self.runtimes.get(pane)
    }

    pub fn is_empty(&self) -> bool {
        self.runtimes.is_empty()
    }

    pub fn get_mut(&mut self, pane: &PaneId) -> Option<&mut PaneRuntime> {
        self.runtimes.get_mut(pane)
    }

    pub fn insert(&mut self, pane: PaneId, runtime: PaneRuntime) -> Option<PaneRuntime> {
        self.runtimes.insert(pane, runtime)
    }

    pub fn remove(&mut self, pane: &PaneId) -> Option<PaneRuntime> {
        self.runtimes.remove(pane)
    }

    pub fn clear(&mut self) {
        self.runtimes.clear();
    }

    pub fn values(&self) -> impl Iterator<Item = &PaneRuntime> {
        self.runtimes.values()
    }
}

impl From<HashMap<PaneId, PaneRuntime>> for PaneRuntimeRegistry {
    fn from(runtimes: HashMap<PaneId, PaneRuntime>) -> Self {
        Self { runtimes }
    }
}

impl IntoIterator for PaneRuntimeRegistry {
    type Item = (PaneId, PaneRuntime);
    type IntoIter = std::collections::hash_map::IntoIter<PaneId, PaneRuntime>;

    fn into_iter(self) -> Self::IntoIter {
        self.runtimes.into_iter()
    }
}

#[cfg(test)]
impl PaneRuntimeRegistry {
    pub fn drain(&mut self) -> impl Iterator<Item = (PaneId, PaneRuntime)> + '_ {
        self.runtimes.drain()
    }
}
