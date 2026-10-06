# Hygiene: dead code

Modules, functions, flags, fields and files nothing uses any more: switches with
one value, compatibility paths for versions nobody runs, leftovers of removed
features (pane history and the recently removed settings among them). Each entry
says what tells the hunter it is dead. Filed from the nine-scope hunt; each entry
names the hunts that reported it.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.

---

## DEAD-001 - History-era files are orphaned on every host that ran with `pane_history`

Reported by: persistence.

Before the pane-history removal, recovery copies came in pairs
(`session-<ts>-<seq>.json` and `session-history-<ts>-<seq>.json`) plus a live
`session-history.json`. History copies could be up to 256 MiB each, across up to 48
snapshots and 3 backups. `RecoveryKey::parse` now rejects `session-history-...` names,
so pruning never sees them, and nothing removes `session-history.json`. Any host that
ran with `pane_history` keeps potentially gigabytes of screen contents in its data
directory indefinitely. The owner never enabled the setting, and the rule is no
migration code, so this is an operator action, not a code change: remove
`session-history.json`, `session-snapshots/session-history-*` and
`session-backups/session-history-*` by hand where present. Filed because nothing else
will ever do it.

## DEAD-011 - Agent detection code with no production reader

Reported by: agent-state, workspace-model.

- `visible_working` end to end (BUG-031).
- `ClientShellAgent.agent` on the wire is still an `Option`, though the server now
  always sends `Some` (`SnapshotAgent.agent` became required).
- `SessionRestoreDamage.repaired_bookmarks` is a count that can only be 0 or 1 (there
  is one bookmark); a `bool` says that.
- `session_saves_stopped` and `session_saves_blocked_on_backup` are two wire booleans
  for mutually exclusive states; one unit enum says that (check the codec rule against
  tagged enums first).
- `bundle.rs` builds dummy params to read an API method name because `shepr-api`'s
  `MethodKind` and its `traits()` are private; a public name lookup by kind is cleaner.

## DEAD-013 - Integration code, keys and assets nothing needs

Reported by: integrations.

- The `pi.events.on("shepr:blocked")` listener in `decoders/pi.ts` and
  `decoders/omp.ts`: an inbound event nothing in shepr emits and nothing documents (its
  payload's `label` field is a remnant). Document it as a feature or delete it.

## DEAD-014 - Workspace model leftovers: misplaced agent types, stale module names, small duplicates

Reported by: workspace-model.

- `shepr-core`'s `agent_state.rs` (`PresentedAgentState`) and `agent_session.rs`
  (`AgentSessionRefKind`) are used by nothing in core; `shepr-agent` re-exports them and
  `shepr-protocol` imports one directly although it may depend on `shepr-agent`.
  AGENTS.md places agent identity in `shepr-agent`. Move them; the `shepr-core-layer`
  rule then keeps core free of agent types.

## DEAD-015 - Server lifecycle types, payloads and arms with one value or no reader

Reported by: server-lifecycle.

- The removed-method and removed-command test lists (`schema/tests.rs`
  `removed_methods_are_rejected`, `removed_uncalled_methods_are_rejected`, `cli.rs`
  `unknown_commands_and_launch_flags_are_rejected` with `--session`, `machine`,
  `integration`, `config`, `remote-api-bridge`, ...) assert that unknown strings are
  unknown. They protect against resurrection, which the owner may value, but grow with
  every removal and fail only on a deliberate re-add.
- Not dead, listed so they are not mistaken for leftovers: the `#[serde(default)]` on
  `Pong.stopping` / `starting` and `StatusOverviewJson.summary`, which AGENTS.md keeps for
  `status --all` against older hosts.
