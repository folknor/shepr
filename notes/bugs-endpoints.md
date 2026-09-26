# Endpoint and SSH defects

```
1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
```

## EP-003 - Saved-machine changes do not reach open clients, although the CLI says they do

- **Claim broken:** `cli/machine.rs` says "Changes apply automatically to open local Shepr clients", "Open Shepr clients connect automatically", and "Open Shepr clients retry within 30 seconds".
- **Actual behaviour:**
  - The client loads `EndpointCatalog` once (`mod.rs`). `EndpointSupervisors::new` only uses the profiles from startup. `set_endpoint_catalog` is only called at startup, and nothing watches the file.
  - Added machines never connect, and removed or disabled ones keep reconnecting.
  - `federated` is also fixed at launch, so a Local-only client stays fatal on Local loss after a machine is added.
  - The `reconnect` message is only true for an endpoint already in `Attention`.
- A `SavedSshConnector` is built per enabled profile at startup; a live-catalog fix should create and drop connectors per profile.

## EP-011 - Bridge socket paths are predictable

- Bridge socket names are pid-derived, so a socket another user leaves at that path can block a connect attempt. The bind is owner-only (`ipc::bind_private_local_listener`) and the bridge accept checks its peer, so this is only a denial of a connect attempt, not an access problem.
