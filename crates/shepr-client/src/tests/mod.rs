//! Test support shared across the crate's unit tests: canonical protocol ids and the
//! endpoint fixtures (a recording transport and the snapshots and surfaces a test server
//! sends). Tests themselves sit with the code they exercise.

use shepr_test_fixtures::{counter_at, fixed_boot_id};

pub(crate) mod endpoints;

/// A public pane id from its canonical spelling (`<workspace>:p<number>`).
/// Test ids go through the parser a server's ids go through, so a test cannot
/// build an id no server would issue.
pub(crate) fn test_pane_id(id: &str) -> shepr_protocol::PublicPaneId {
    id.parse()
        .unwrap_or_else(|_| panic!("{id:?} is not a canonical public pane id"))
}

/// A workspace id from its canonical spelling (`w<number>`).
pub(crate) fn test_workspace_id(id: &str) -> shepr_protocol::WorkspaceId {
    id.parse()
        .unwrap_or_else(|_| panic!("{id:?} is not a canonical workspace id"))
}

/// The canonical boot id of the test server named `name`. Tests name the
/// servers they talk to; each name maps to its own boot id, so two names never
/// share one and a test cannot build a boot id no server would send.
pub(crate) fn test_boot_id(name: &str) -> shepr_protocol::BootId {
    let process_id = match name {
        "boot" => 1,
        "boot-1" => 2,
        "local-boot" => 3,
        "remote-boot" => 4,
        "old-boot" => 5,
        "new-local-boot" => 6,
        "replacement-boot" => 7,
        "stale-local-boot" => 8,
        "shared-server-boot" => 9,
        "restarted-remote" => 10,
        "restarted-local" => 11,
        "restored" => 12,
        "restored-first" => 13,
        "restored-second" => 14,
        "saves-stopped" => 15,
        "saves-stopped-next" => 16,
        "boot-2" => 17,
        "rebooted" => 18,
        "other-boot" => 19,
        "bookmark-repaired" => 20,
        _ => panic!("{name:?} names no test server"),
    };
    fixed_boot_id(process_id)
}

/// The connection generation at position `position` of the test's own
/// numbering: tests name generations by number the way they name servers.
pub(crate) fn test_generation(position: u64) -> shepr_protocol::ConnectionGeneration {
    counter_at(position)
}
