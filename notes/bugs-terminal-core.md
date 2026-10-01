# Defects: terminal core (shepr-vt, shepr-pty)

Filed from the defect hunt over `crates/shepr-vt/src/` and `crates/shepr-pty/src/`.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## TCORE-001 - A widened pane loses scrollback and every absolute row id when its height changes

Location: `Terminal::resize`, `crates/shepr-vt/src/lib.rs`. Hunter's severity:
medium.

Claims broken: the comment in `resize` says lowering the history limit "at most
to the history already held, never below it" means a zoom/unzoom cycle cannot
"destroy scrollback for good". `history_origin`'s doc says an absolute id names
the same line for as long as that line is retained.

Mechanism. After a widening resize, `history_lines` deliberately stays above the
byte budget for the new width (retained limit H, budget B < H). The next resize
that changes only the height takes the non-rewrap path:

```rust
let history_lines = budget_lines.max(self.term.history_size().min(self.history_lines));
```

This is evaluated after `self.term.resize`. A height grow of k rows makes
alacritty's `Grid::grow_lines` pull k lines out of history into the screen, so
`history_size()` is now H - k, and the limit is lowered to H - k (when above B).

- Row ids are wiped at the grow. `rows.finish` runs with the lowered limit.
  History (H - k) equals the limit, so the "history below its limit" early
  return does not fire. The anchor `begin` picked was the newest history row
  (`Line(-1)`); it is now on screen at `Line(k - 1)`, but the walk only looks
  from `Line(-1)` upward, never finds it, and falls through to
  `self.evict(anchor.total)`, invalidating every absolute id. Copy-mode cursor,
  selection and search matches jump or vanish though no line was evicted.
- Scrollback is lost at the shrink back. `max_scroll_limit` is now H - k. When
  the height shrinks back by k, `Grid::shrink_lines` scrolls k lines into
  history; `increase_scroll_limit` has no room (`min(k, (H-k) - (H-k)) = 0`), so
  the rotation recycles the k oldest history lines.

Reachable in normal use: a pane with more history than its byte budget at a
wider width (a wider client attaching, a zoom, closing a side split) followed by
any height change (closing a split below, a taller client). The test
`widening_resize_keeps_history_that_already_fit` only cycles the width.

Fix direction: only lower the limit on a column change, or base the floor on
lines retained (history plus the screen rows that came out of history) rather
than `history_size()` alone. Lowering must also happen before `rows.finish`, or
`finish` must take the pre-resize limit. Cleaner rewrite: give the limit one
owner, a "retained lines" floor that only RIS, ED 3 and the host clear may
lower, so `resize` never lowers it.

## TCORE-002 - `ChildIo::shutdown` drops writes queued before it, not only after

Location: `crates/shepr-pty/src/child_io.rs` (doc) and `actor.rs` (`run`,
`close_inbox`). Hunter's severity: low (doc/behaviour mismatch).

The trait doc says "Stop accepting writes; anything queued afterwards is
dropped", which reads as if writes queued before the call still go out. The
actor checks `shutdown` at the top of its loop before `pump`, then
`close_inbox` clears `entries` and `latest_resize`, so input and replies already
queued are discarded too. The only caller today is `PaneRuntime`'s `Drop`, where
discarding is right. The doc should say "everything still queued is dropped", or
a caller relying on the stated semantics loses its last write.

## TCORE-003 - Doc drift: `PtyCommand` has no argv launch

Location: `AGENTS.md`, "Terminal core": "`command.rs` (`PtyCommand`: argv or
login shell, full env control, cwd) builds the launch". The hunt brief repeated
it.

`PtyCommand` has one constructor, `interactive_shell(program, login)`, and no
way to pass arguments. It runs the configured shell (login argv0 or plain) with
no argv beyond argv0. Agent resume also spawns the shell (`agent_resume.rs` uses
`PaneShellConfig::new(...).require_cwd()`). The doc should say "the pane shell,
login or not".

## TCORE-004 - Pane spawn does blocking filesystem work on the server event loop

Lateral, filed by the hunter as a design observation rather than a broken
claim. `PtyCommand::to_std_command` stats the requested cwd (`usable_directory`)
and walks `PATH` (`resolve_executable` with `classify_candidate`, a `stat` plus
`access` per candidate) on the calling thread, and the parent's
`Command::spawn` waits for the child's chdir and exec. Every pane spawn and
restore runs this on the server's event loop, so a hung mount in the cwd,
`HOME` or any `PATH` entry stalls the whole server, not just resume. This is
wider than the note in `agent_resume.rs` acknowledges.

## TCORE-005 - Empty `impl PtyReadResult {}` in production code

Location: `crates/shepr-pty/src/actor.rs`, right after the struct. A leftover
next to the test-only `impl PtyReadResult { fn empty() }`.

## TCORE-006 - `mark_inherited_fds_cloexec` fallback leaks `directory_fd` on error

The fallback returns early on getdents or record-parse errors without closing
`directory_fd`. Harmless, because an error fails the spawn and the forked child
exits; noted because the rest of the function is careful about ownership.

## TCORE-007 - The scanner over-counts OSC parser bytes

`osc_parser_bytes` counts the `;` separators, which vte keeps out of `osc_raw`
(`action_osc_put_param` does not push them), so the cut comes a few bytes early.
The 2x headroom in `MAX_PARSER_OSC_BYTES` keeps the "every accepted OSC 52 store
reaches the parser whole" claim true, so not a behaviour defect, but the bound is
not quite "body bytes handed to the parser" as named.

## TCORE-008 - Alternate-screen row ids reuse primary ids

On the 1049 switch the origin does not move, so `origin + y` names a primary
line before the switch and an alt line after it. `history_origin`'s doc states
this, and the client drops selections and copy-mode state when
`alternate_screen_active` flips between two surfaces. A consumer that only
compares origins would not notice the switch.
