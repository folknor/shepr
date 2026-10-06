use super::*;

impl TerminalState {
    // Persistence tests model malformed authority which no report accepts.
    pub(crate) fn seed_hook_authority_for_test(&mut self, authority: Option<HookAuthority>) {
        self.ownership = std::mem::take(&mut self.ownership).with_initial_hook_authority(authority);
    }
}
