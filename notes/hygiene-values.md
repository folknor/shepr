# Hygiene findings: values and their owners

This file collects the findings from the nine-scope hygiene hunt that answer the
hunt's first two questions: (1) values spelled at more than one site instead of
being defined once and read - environment variables and their resolution rules,
tunable constants and thresholds, ports, endpoints and addresses, filesystem
paths and roots, timeouts and limits, exit codes and status strings; and (2)
values nobody can find, change or trust - a knob defined once but where nobody
tuning the system would look, a value with no injection point, configuration read
at the moment of use rather than validated once at startup. Entries gather every
site and every hunter that reported the same value. It is a working document
assembled from nine independent readings, none of them verified by running the
build, so individual entries may be wrong; a later fix pass is expected to find
phantoms here.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

---

## HYGV-150 - Limits restated or left inline after the limits move

- `shepr-client/src/shell/sidebar/sidebar_tokens.rs` declares local `MIN` and
  `MAX` constants that alias the `shepr-core` split limits; read the core
  constants directly.
- `shepr-platform/src/ssh_paths.rs` spells the shared SSH directory mode as an
  inline `0o700` beside the named `PRIVATE_SOCKET_MODE` in `ipc.rs`.
- The 256-colour palette size is spelled four ways: termio
  `HOST_PALETTE_COLOR_COUNT`, mux `TERMINAL_PALETTE_COLORS`, protocol
  `MAX_CLIENT_HOST_PALETTE_COLORS` and vt's `default_palette() -> [RgbColor;
  256]`. The UTF-8 four-byte width is two constants (agent, termio), and
  `KIBIBYTE_BYTES` is defined in both vt and pty. Give each one owner low in
  the layering.
- Several limits are trivial `= 1` floors (`MIN_BUFFER_ROW_LEN`,
  `LIMITED_READ_OVERFLOW_PROBE_BYTES`, `MIN_POLL_TIMEOUT_*`) that name no
  tunable; fold them back into the code that needs them, with a comment.
- `shepr-protocol`'s `pub use limits::*` now also exposes `DEFAULT_MAX_DEPTH`
  and `MAX_COLLECTION_ITEMS` at the crate root beside `codec::`; re-export by
  name.

## HYGV-151 - Limits doc comments restate the value in prose

Many new `limits.rs` doc comments spell the value out ("Four thousand
ninety-six characters", "350 milliseconds"), mostly in the client, server and
platform modules. The prose drifts the moment the number changes; say what the
bound is for and why it is that size, not what it is.

## HYGV-043 - The clipboard byte caps have two unrelated owners, and one is restated as a magic number in its own test

The platform half is resolved: `MAX_CLIPBOARD_TEXT_BYTES` is at module scope in
`crates/shepr-platform/src/clipboard.rs` and its test derives the oversize input
from it. The two caps stay separate on purpose: the 1 MiB platform cap bounds
host clipboard reads, the 192 KiB `shepr-vt` cap bounds terminal-originated OSC
52 stores, opposite directions.

Open: `shepr-vt`'s `MAX_CLIPBOARD_BYTES` drops an OSC 52 payload over 192 KiB
with no log line, so a copy from a pane that silently does nothing cannot be
diagnosed. `shepr-vt` has no `tracing` dependency and returns clipboard effects
as data, so the fix is for it to return the dropped store's byte count as an
effect and for the pane layer in `shepr-mux` to log it, rate-limited, never the
content.

## HYGV-072 - Three boolean-from-string parsers, no owner, and one is an incomplete implementation of an external grammar

**Decision (partial):** the `env_bool()` half is piece 1 (the `shepr-core`
environment registry, after broadarrow's `core::env`): shepr env flags have one
kind and one rule, exactly `1`/`0`/`true`/`false`, so `osc.rs`'s
`"1" | "true" | "yes" | "on"` goes (and `yes`/`on` become refusals). The
`git_bool()` half is resolved: `git/config.rs::git_config_bool` follows git's
grammar (checked against git 2.53) and serves `core.bare` and
`worktree_config_enabled`. Open: the env half, until piece 1 lands at `osc.rs`.

Reported by the mux hunter.

- `crates/shepr-mux/src/git/config.rs`: `"true" | "1" | "yes" | "on"` (Git's
  boolean syntax).
- `crates/shepr-mux/src/pane/osc.rs`: `"1" | "true" | "yes" | "on"` (a shepr env
  var).
- `crates/shepr-remote/src/remote/server_lifecycle.rs`: `"y" | "yes"` (a
  prompt).

The first two are the same list in a different order for two different domains.
Git's actual boolean syntax also accepts the empty string as true and
`off` / `no` / `false` as false, so the git copy is an incomplete implementation
of a documented external grammar while the osc copy is shepr's own invention that
happens to look identical.

This is two owners rather than one: a `git_bool()` in the git module completed
against Git's grammar, and one `env_bool()` wherever shepr env flags are
resolved. Holdable by a text rule forbidding the bare list elsewhere.

## HYGV-087 - Identifier allocation reaches process-global counters and clocks directly, with no injection point and no owner of the format

Reported by the core/platform, protocol/config, remote and server hunters.

- `crates/shepr-core/src/layout.rs`: `static NEXT_PANE_ID`. `PaneId::alloc()`
  reads it, and `alloc_from(&counter)` exists purely so the exhaustion test can
  inject one. Any test wanting deterministic pane ids must use `from_raw`, which
  bypasses validation entirely: it accepts `0`, the documented placeholder, while
  `collect_validated_ids` rejects `0`. Fix: `PaneId::from_raw -> Option<PaneId>`
  is a compiler-enforced signature change; removing the global needs an allocator
  value threaded through `Workspace`, which is the larger and better fix.
- `crates/shepr-protocol/src/ids.rs`'s doc claims `TerminalId` is an "opaque
  identity for a server-owned terminal ... callers must not derive it from a pane
  id or layout position", while `TerminalId` has a public `From<String>` and a
  non-`cfg`-gated `pub fn test_new`, so deriving one from anything is a one-liner.
  Removing `From<String>` and gating `test_new` makes the claim structural.
