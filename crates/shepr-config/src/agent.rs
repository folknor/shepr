/// Agent identity shared with detection, integrations, resume and presentation.
pub use shepr_agent::agent::Agent as ConfigAgent;

/// Parse canonical agent names and declared aliases without process-name normalization.
pub(crate) fn parse_config_agent(value: &str) -> Option<ConfigAgent> {
    let name = value.trim();
    ConfigAgent::all().find(|agent| {
        agent.label().eq_ignore_ascii_case(name)
            || agent
                .descriptor()
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(name))
    })
}
