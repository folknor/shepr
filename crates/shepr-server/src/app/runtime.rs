use super::App;

impl App {
    pub(crate) fn shutdown_pane_runtime(&mut self, pane_id: shepr_core::layout::PaneId) {
        if let Some(runtime) = self.terminal_runtimes.remove(&pane_id) {
            drop(runtime);
        }
    }

    /// Shuts down the runtimes of panes a state removal detached.
    pub(crate) fn shutdown_detached_pane_runtimes(
        &mut self,
        pane_ids: &[shepr_core::layout::PaneId],
    ) {
        for pane_id in pane_ids {
            self.shutdown_pane_runtime(*pane_id);
        }
    }
}
