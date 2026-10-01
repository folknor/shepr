# Defects: wire and config

Filed from the defect hunt over `crates/shepr-protocol/src/`,
`crates/shepr-api/src/`, `crates/shepr-config/src/` and the build identity
script.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

## WIRECFG-001 - The TUI client validates server-owned settings in its own context and refuses to launch on them

Files: `crates/shepr-config/src/validated.rs` (`ConfigResolution::parse` always
runs `ValidatedTerminalConfig::parse`: `resolve_default_shell`,
`parse_new_cwd`), `crates/shepr-config/src/io.rs` (`Config::load_validated`).

Claim: AGENTS.md: "A setting belongs to whoever draws or interprets it ... Each
server applies its own config to what it runs ...: shell and working directory".

The client launch validates `terminal.default_shell` against the client's
`PATH`/`SHELL` and `terminal.new_cwd` against the client's current directory. It
checks the directory exists and that `SHELL` names a shell shepr recognises.
Nothing in `shepr-client` or `src/` reads `config.terminal()` (checked with
grep). So running `shepr` with an unrecognised `SHELL` (a dev environment that
sets `SHELL` to a wrapper, or an uncommon shell) fails the TUI launch even when
it only attaches to a running local server or to remote machines; and with a
relative `terminal.new_cwd`, launching from a directory without that
subdirectory fails, though the running server resolved the setting at its own
launch and never re-resolves it for this client.

Owner's decision: split the file itself. `config.toml` is replaced by two files
in the XDG config directory, `client.toml` read only by `shepr` (the TUI and its
internal client launch) and `server.toml` read only by `shepr-server`. Neither
program reads the other's file, and each stays strict: an unknown key, including
a setting placed in the wrong file, fails that program's launch. The division
follows "a setting belongs to whoever draws or interprets it":

- `client.toml`: `[[machines]]`, `[keys]`, the client-drawn `[ui]` settings
  (sidebar width, bounds, collapse and mode, mouse capture, copy on select, host
  cursor, right-click passthrough, redraw on focus, scroll lines, close and name
  prompts, agent panel sort, status indicators) and the CJK IME cursor settings.
- `server.toml`: `[terminal]`, `[session]`, `[server]`, `[advanced]`,
  `pane_history`, and the `[ui]` settings the server draws into pane cells (pane
  borders, outer borders, scrollbars, gaps, agent labels on borders, window
  title).
- `[theme]` appears in both: the client colours its own chrome, the server its
  pane chrome.

Also decided: remove `experimental.allow_nested`. The same-profile nested launch
refusal in `src/main.rs` becomes unconditional; a dev client inside a release
pane stays allowed as now.

Update AGENTS.md (the Config paragraph), `docs/`, `reference/`, the bundled
default config and `brokkr man config` to match. Nothing on disk needs
migrating (shepr has never been run). The change threads role-specific config
types through consumers in several crates (shepr-config, shepr-client `lib.rs`
and its presentation config readers, shepr-server `app/mod.rs`, `app/state.rs`
and `headless/bootstrap.rs`, `src/main.rs`). Run it as a single-fixer wave.

## WIRECFG-004 - Full surface renders still build, compare and clone the whole grid

The full-surface size count is gone (cell deltas use a five-byte-per-cell lower
bound and count only the candidate update), metadata-only updates skip counting
and the grid comparison, compact sends move the rendered grid into the
committed baseline, and projection decoding makes one grid copy instead of two.
Residue: a full render still builds the grid and compares it with the last one
(`last.frame == surface.frame`), a full send still clones it for the committed
baseline, projection decoding keeps one copy for its separate consumer, and the
conservative lower bound can pass over a delta that would have paid.
