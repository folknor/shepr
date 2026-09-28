# Porting broadarrow's mechanical checks to shepr

Which of `research/broadarrow`'s textlints, presets, script checks, extra sweeps
and clippy seals shepr adopts, drafted for shepr's paths. The A items are pure
hygiene that enforces a rule `AGENTS.md` / `CLAUDE.md` already states; the B
items are seals that change how code must be written. The owner has ruled on
every item the decision round covered, so the file is a work list. Each item
carries a **Decision:** line with one of four outcomes:

- **Adopted now:** land the draft (re-run its sweep first; counts drift).
- **Adopted incrementally:** land it as part of the hygiene work, crate by crate
  or subsystem by subsystem, not wholesale.
- **Rejected:** decided against; do not re-propose.
- **Tracked elsewhere:** owned by another work item, named in place.

An item that has landed is marked **Done** after its decision line.

At a glance:

| Item | Decision | Status |
|---|---|---|
| `_brokkr_config.py` port, A1-A7, A9, A10 | Adopted now | Done |
| A8 `#[allow]` carries a comment | Dropped (superseded by B9) | - |
| B1 print macros (library-crate textlint) | Adopted now | |
| B2 `process::exit` only in `src/main.rs` | Adopted now | |
| B3 `debug_assert!` ban, `debug_assertions` switch removed | Adopted now | |
| B4 `Path::exists`, `Path::is_file`, `Path::is_dir` seal | Adopted now | |
| B5 `catch_unwind` seal | Adopted now | |
| B6 child working-directory seal (clippy form) | Adopted now | |
| B7 clock seam | Adopted incrementally | |
| B8 per-crate `limits` modules | Adopted incrementally | |
| B9 `#[expect(.., reason)]` instead of `#[allow]` | Adopted now | |
| B10 extra compiler lints and the rustdoc phase | Adopted, later round | |
| B11 release-profile sweep in `brokkr check` | Rejected; release gets `overflow-checks = true` instead | |
| B12 install shape | Tracked elsewhere (piece 4 of the test-isolation work) | |
| B13 tree debris | Adopted, later round | |
| B14 scripts roster | Adopted, later round | |
| B15 workspace dependency pins | Adopted, later round | |
| B16 tests do not read compiled content | Adopted, later round | |
| B17 seal witness | Adopted, later round | |

Two related decisions from the same round: the `research/alacritty` and
`research/vte` citations are reworded to point at the pinned sources in the
cargo registry (A7), and `LLM.md` is out of scope and stays as it is (surprise
10).

Already decided and implemented elsewhere, so not drafted here: the
environment-reader seal (`std::env::var*`, `set_var`, `remove_var`, and the
`disallowed-escapes-are-allowlisted` textlint), and the test-isolation pieces
(scratch dirs and the `temp_dir` ban, host-program stand-in bans, never-ships
dependency rules). Broadarrow-domain rules (Nautilus, venues, Pine, piners,
daemon/worker protocol, `BaVenueId`, limits specific to the bridge) are out.

## How the counts were taken

- Read-only `rg` sweeps over the tree as it stood during this pass. Two other
  agents are editing code, so treat every count as a snapshot and re-run the rule
  before applying it.
- Rust `region = "comment"` rules were approximated by matching full-line
  comments (`//`, `///`, `//!`) only. Trailing comments after code were not swept,
  so a real run can find a few more.
- Every textlint key used below was checked against `brokkr man config textlint`
  and `brokkr man config textlint_preset`: `name`, `pattern`, `paths`, `exclude`,
  `except`, `message`, `region`, `allow_marker`, `allow_marker_above`,
  `skip_after`, `preset`. Script checks use `name`, `command`, `expect`,
  `match`, `stage`. Check sweeps use `packages`, `profile`, `only`, `curated`.
  `[bin]` uses `install` and `install_feature_check`. All documented.
- Broadarrow also uses `except_above` and `only_if_file_matches_above`. Neither is
  in `brokkr man config`, but both exist in brokkr's source
  (`src/textlint.rs`, `src/config_parts/schema.rs`, `src/config_parts/parser.rs`),
  so shepr's brokkr supports them; they are just undocumented. None of the drafts
  below needs them.

## Shared prerequisite for the script checks

**Decision:** adopted now, ahead of the first script check (A7, A9).

**Done.** `scripts/_brokkr_config.py` expands glob members, returns the root
package's manifest, adds `.local` and `__pycache__` to `PRUNED_DIRS`, and drops
`install_packages()`.

Every ported script imports `scripts/_brokkr_config.py`. Port it with one change:
`workspace_member_manifests()` raises on a glob member, and shepr's workspace is
`members = ["crates/*"]`, so it needs to expand globs. It should also return the
root package's manifest, because shepr's root `Cargo.toml` is itself a package
(the `shepr` binary) and would otherwise be skipped.

```python
def workspace_member_manifests() -> list[pathlib.Path]:
    """Manifest paths the root workspace names, root package included."""
    with (ROOT / "Cargo.toml").open("rb") as stream:
        root = tomllib.load(stream)
    paths = [ROOT / "Cargo.toml"] if "package" in root else []
    for member in root["workspace"]["members"]:
        matches = sorted(ROOT.glob(member)) if any(c in member for c in "*?[") else [ROOT / member]
        if not matches:
            raise RuntimeError(f"workspace member {member!r} matches nothing")
        for directory in matches:
            path = directory / "Cargo.toml"
            if not path.is_file():
                raise RuntimeError(f"workspace member manifest is missing: {path}")
            paths.append(path)
    return paths
```

`PRUNED_DIRS` already covers shepr's untracked directories (`.brokkr`, `.plans`,
`research`, `target`, `.claude`). `install_packages()` is only used by scripts not
proposed here; drop it or keep it for a later `[bin] install`.

Drop the `SPDX` header lines when copying: shepr is Apache-2.0 and carries no
per-file headers.

---

# A: pure hygiene, all adopted now

## A1. No shouting

**Decision:** adopted now.

**Done**, as drafted, including the two violations handed to the
scratch-directory work (`crates/shepr-platform/src/tests.rs` and
`crates/shepr-remote/src/remote/attach.rs`).

**Catches** all-caps emphasis words in comments and docs. `CLAUDE.md` states
"Shouting is illegal. No all-caps words for emphasis." Nothing enforces it today,
and the tree has live instances, including a doc comment that shouts two words at
the next maintainer.

**Draft.** The preset carries shepr's vocabulary in place of broadarrow's venue
tickers: terminal control sequences (`DECSCUSR`, `XTGETTCAP` and kin), ioctls and
poll flags, errno names, and quoted all-caps literals (screen text in detection
manifests, the preamble magic). `AND/OR/NOT` is detection-gate vocabulary that
`AGENTS.md` uses itself.

```toml
[textlint_preset.no-shouting]
allow_marker = "shout-ok"
allow_marker_above = 2
except = [
    '[$%]\{?[A-Z][A-Z0-9_]*',
    '\b[A-Z][A-Z0-9_]{3,}\s*[:=]',
    '\b(?:env!|env::var(?:_os)?|getenv|environ|EnvVar::)',
    'https?://',
    '^\s*(?://[/!]?|\*|#+)?\s*(?:SAFETY|PANICS|ERRORS|INVARIANT|WARNING)\b',  # shout-ok: pattern text
    '"\[?[A-Z][A-Z0-9_]{4,}\]?"',
    '\b(?:ASCII|UTF|POSIX|JSONL|README|LICENSE|MSRV|TOCTOU|COLORTERM)\b',
    '\bSIG[A-Z0-9]+\b',
    '\b(?:ESRCH|EPIPE|ECONNRESET|ECONNREFUSED|ECONNABORTED|EAGAIN|EWOULDBLOCK|ELOOP|ETXTBSY|EACCES|EADDRINUSE|EBUSY|ENOSPC|ENOENT|EINVAL|EPERM|EINTR|EBADF|ECHILD|ENOTDIR|EISDIR|EMFILE|ENFILE|ETIMEDOUT|ENOTCONN|ENOPROTOOPT|EOVERFLOW|EFBIG|EFAULT|ENOTTY)\b',
    '\b(?:DECRQM|DECSCUSR|DECSC|DECRC|DECSED|DECCOLM|DECCKM|DECSET|DECRST|XTGETTCAP|XTMODKEYS|XTWINOPS|XTVERSION)\b',
    '\b(?:TIOCSCTTY|TIOCSWINSZ|TIOCGWINSZ|POLLHUP|POLLERR|WEXITED|O_NONBLOCK)\b',
    '\bAND/OR(?:/NOT)?\b',
    '\b(?:AGENTS|CLAUDE)\.md',
]

# The short-word list is the words already swept, not every short shout. Widen
# it one word at a time, sweeping the tree first.
[[textlint]]
name = "no-shouting-rust"
preset = "no-shouting"
pattern = '^[^`]*(?:`[^`]*`[^`]*)*\b(?:[A-Z]{5,}|ALL|BOTH|MUST|NEVER|NOT|ONE|ONLY|SAME)\b'
paths = ["crates/**/*.rs", "src/**/*.rs", "build.rs"]
region = "comment"
message = "write the emphasis into the sentence; don't shout in all caps (shout-ok to allow)"

[[textlint]]
name = "no-shouting-hash-comments"
preset = "no-shouting"
pattern = '#[^`]*(?:`[^`]*`[^`]*)*\b(?:[A-Z]{5,}|ALL|BOTH|MUST|NEVER|NOT|ONE|ONLY|SAME)\b'
paths = [
    "Cargo.toml",
    "brokkr.toml",
    "clippy.toml",
    ".review.toml",
    "crates/**/*.toml",
    "crates/**/*.sh",
    "crates/**/*.py",
    "crates/**/*.yaml",
    "scripts/**/*.py",
]
message = "write the emphasis into the sentence; don't shout in all caps (shout-ok to allow)"

[[textlint]]
name = "no-shouting-script-comments"
preset = "no-shouting"
pattern = '//[^`]*(?:`[^`]*`[^`]*)*\b(?:[A-Z]{5,}|ALL|BOTH|MUST|NEVER|NOT|ONE|ONLY|SAME)\b'
paths = ["crates/**/*.ts", "crates/**/*.js"]
message = "write the emphasis into the sentence; don't shout in all caps (shout-ok to allow)"

[[textlint]]
name = "no-short-shouting-markdown"
preset = "no-shouting"
pattern = '^[^`]*(?:`[^`]*`[^`]*)*\b(?:ALL|BOTH|MUST|NEVER|NOT|ONE|ONLY|SAME)\b'
paths = ["**/*.md"]
exclude = ["research/**", "notes/**", "AGENTS.md", "CLAUDE.md"]
except = [
    '^ {4,}\S',
    '^\t',
    '^\s*"?[a-z_][a-z0-9_.-]*"?\s*[:=]\s*\S',
    '^\s*\{"',
    '^\s*(?:\$|#\s)?\s*(?:shepr|brokkr|cargo|git|ssh|export)\b',
    '^\s*Usage:\s',
]
message = "write the emphasis into the sentence; don't shout in all caps (shout-ok to allow)"

[[textlint]]
name = "no-shouting-markdown"
preset = "no-shouting"
pattern = '^[^`]*(?:`[^`]*`[^`]*)*\b[A-Z]{5,}(?:-[^0-9]|[^\w-]|$)'
paths = ["**/*.md"]
exclude = ["research/**", "AGENTS.md", "CLAUDE.md"]
except = [
    '^ {4,}\S',
    '^\t',
    '^\s*"?[a-z_][a-z0-9_.-]*"?\s*[:=]\s*\S',
    '^\s*\{"',
    '^\s*(?:\$|#\s)?\s*(?:shepr|brokkr|cargo|git|ssh|export)\b',
    '^\s*Usage:\s',
]
message = "write the emphasis into the sentence; don't shout in all caps (shout-ok to allow)"
```

The `# shout-ok: pattern text` marker on the `SAFETY` except line is needed
because `no-shouting-hash-comments` scans `brokkr.toml` itself and that line
carries `#+`. Broadarrow needs the same marker for the same reason.

**Violations (13).**

Real shouting, reword (4):

- `crates/shepr-server/src/server/headless/internal_events.rs:7` (`ALL` ... `MUST`)
- `crates/shepr-termio/src/blit.rs:1587` (`NOT`)
- `crates/shepr-mux/src/pane/terminal/tests.rs:3495` (`NOT`)
- `crates/shepr-remote/src/remote/attach.rs:297` (`BEFORE`)

Identifiers written bare in prose; backtick them or reword (9):

- `crates/shepr-config/src/model.rs:180` (`SHELL`, write `$SHELL`)
- `crates/shepr-pty/src/command.rs:156` (`SHELL`)
- `crates/shepr-platform/src/tests.rs:554` (`DISPLAY`)
- `crates/shepr-mux/src/persist/restore.rs:829` (`SHEPR` identity environment)
- `crates/shepr-agent/src/integration/config_edit.rs:881` (`BEGIN` marker)
- `crates/shepr-mux/src/pane/terminal/tests.rs:1244` (`DISAMBIGUATE`)
- `crates/shepr-client/src/shell/sidebar/agent_sidebar.rs:570` (Unicode name `COMBINING ACUTE ACCENT`)
- `crates/shepr-config/src/default.toml:240` (`#RGB`/`#RRGGBB`)
- `crates/shepr-agent/src/integration/assets/shepr-agent-state.test.ts:246` (`OMPCODE`)

Markdown: 0. Every all-caps word in `notes/` and `LLM.md` is covered by the
preset.

## A2. Code does not cite the agent instruction files

**Decision:** adopted now. **Done**, as drafted.

**Catches** `AGENTS.md` / `CLAUDE.md` citations from code and config. Shepr's
documentation rule says a code comment must carry its full context; the agent
files are instructions to agents, can be reworded by the owner at any time, and
are not a durable home for a reason.

```toml
[[textlint]]
name = "code-does-not-cite-agent-instructions"
pattern = '\b(?:AGENTS|CLAUDE)\.md\b'
paths = ["crates/**", "src/**", "build.rs", "Cargo.toml", "clippy.toml"]
message = "code does not cite the agent instruction files; state the reason in place"
```

`brokkr.toml` is left out of `paths` on purpose: the rule's own pattern line would
match.

**Violations (1):** `crates/shepr-agent/src/detect/proc_tree.rs:30` ("see
AGENTS.md multiplicative performance paths"). State the hot-path argument inline.

## A3. No pre-deployment older-peer compatibility

**Decision:** adopted now. A violation is cleared by deleting the compatibility
code it sits on, not by rewording the comment; the log-tightening violation
(formerly HYGP-040, now resolved and removed) and HYGP-036 (the
finished-migration prose) in `notes/hygiene-policy.md` carried two of them.

**Done**, as drafted. The log-permission tightening in `logging.rs` and its
test half are deleted; the `lines` comment in `snapshot.rs` is gone (no code
sat under it: serde ignores unknown fields by default). Three flagged comments
sat on current behaviour and were reworded instead: `persist/io.rs` (atomic
publish replaces any broader-mode file, whoever made it), the OpenCode
two-file test (OpenCode reads both files, so a hand-added registration can sit
beside shepr's) and `terminal/state/mod.rs`. The test
`legacy_preferences_ignore_unknown_fields` in `preferences.rs` is deleted too.
`migration_tests.rs` is left alone: it is a terminal-core
behaviour harness, not on-disk compatibility.

**Catches** comments reasoning about older shepr builds, installs or on-disk
shapes. `AGENTS.md`: "shepr has never been run ... nothing to stay compatible
with. Remove legacy fields and migration code freely." A comment that argues for
an older build is usually sitting on code that exists only for it.

```toml
[[textlint]]
name = "no-predeployment-older-peer-compatibility"
pattern = '(?i)\b(?:older|old|previous|earlier|legacy|pre-fork)\s+(?:builds?|binar(?:y|ies)|installs?|installations?|peers?|schemas?|shepr|herdr)\b|\bduring the migration\b|\blegacy (?:preferences|fields?|files?|sessions?)\b'
paths = ["crates/**/*.rs", "src/**/*.rs"]
region = "comment"
message = "shepr has never been run: no older build, install or on-disk shape exists to stay compatible with; describe the current invariant or remove the code"
```

Deliberately not matched: "old server" (a real runtime case: an in-place upgrade
on a remote host leaves the previous server running, `src/cli/server.rs:55` and
`crates/shepr-remote/src/remote/local_server.rs:72`), and "legacy format" (kitty's
term of art for the legacy key encoding, `crates/shepr-termio/src/input/encode.rs:249`).

**Violations (6):**

- `crates/shepr-mux/src/persist/snapshot.rs:201` - "Files written by older builds
  also carry a `lines` count; nothing read it, and serde skips it on load." The
  comment documents tolerance for a shape no build ever wrote to a real disk.
- `crates/shepr-platform/src/logging.rs:558` - production code at 557-561 tightens
  a log "left behind by an older build that created logs world-readable".
- `crates/shepr-platform/src/logging.rs:815` - the test for it.
- `crates/shepr-mux/src/persist/io.rs:421` - test for an older build's
  default-permission history file.
- `crates/shepr-agent/src/integration/tests.rs:1533` - "Older installs may have
  registered the same plugin in both files." Check whether "installs" means
  OpenCode's or shepr's; if shepr's, the dedup code it tests is migration code.
- `crates/shepr-mux/src/terminal/state/mod.rs:187` - "During the migration this is
  still one-to-one ...": transient plan state in a durable doc comment.

Not caught by the rule but the same shape, from the same sweep:
`crates/shepr-client/src/shell/overlays/preferences.rs:129`, test
`legacy_preferences_ignore_unknown_fields`, and
`crates/shepr-mux/src/pane/terminal/migration_tests.rs` ("Bounded semantic
migration gates ... old/candidate captures"), a migration harness named as one.

## A4. No plan or issue labels in durable text

**Decision:** adopted now, both rules. `raw_input.rs`'s `Issue #3911` comment was
also HYGG-052 in `notes/hygiene-guards.md`, now resolved and removed.

**Done**, both rules as drafted; the eight issue citations are removed.

**Catches** work-item labels from `notes/` in code, and upstream tracker numbers.
`AGENTS.md`: nothing durable may cite `notes/`, and a code comment must carry its
full context because it outlives the note. A label like `HYGG-042` or `BUG-049`
is a citation of `notes/` in disguise. An issue number like `#3283` points at
herdr's tracker, which a hard fork with "no compatibility with upstream herdr" no
longer has.

Shepr's label families, read from `notes/`: `BUG-NNN`, `HYGG-`, `HYGV-`, `HYGP-`,
`HYGC-NNN`, `CMD-NNN`.

```toml
[[textlint]]
name = "durable-text-has-no-plan-labels"
pattern = '\b(?:BUG|HYG[GVPC]|CMD)-\d+\b|\b(?:[Rr]ound|[Pp]hase|[Ww]ave|[Mm]ilestone)\s+\d+[a-z]?\b|\b[Ss]pec\s+[A-Z0-9]\b|\bfinding-\d+\b'
paths = ["crates/**", "src/**", "build.rs", "Cargo.toml", "clippy.toml"]
message = "a work-item label means nothing once its note is gone; state the invariant in place"

[[textlint]]
name = "no-upstream-issue-citations"
pattern = '(?i)\bissue\s+#\d+|\(#\d+\)|\bon #\d+\b'
paths = ["crates/**", "src/**", "build.rs"]
message = "an issue number names herdr's tracker, which shepr does not have; state the behaviour the issue described"
```

**Violations.** Plan labels: 0 (tripwire). Issue citations: 8 in 7 files:

- `crates/shepr-agent/src/detect/manifests/claude.toml:123` (`issue #3283`)
- `crates/shepr-agent/src/detect/manifests/claude.toml:177` (`(#2650)`)
- `crates/shepr-server/src/app/api/agents.rs:469` (`reported on #3225 by rszrszrsz`)
- `crates/shepr-agent/src/integration/assets/shepr-agent-state.test.ts:583` (`issue #2851`)
- `crates/shepr-termio/src/input/raw_input.rs:537` (`(#549)`)
- `crates/shepr-termio/src/input/raw_input.rs:2524` (`Issue #3911`)
- `crates/shepr-termio/src/input/encode.rs:1532` (`issue #769`)
- `crates/shepr-mux/src/pane/runtime.rs:1619` (`issue #3270`)

`crates/shepr-client/src/shell/input/word_bounds.rs:384` (`"refs #123 ..."`) is
test data and does not match either alternative.

## A5. No relative dot-directory paths

**Decision:** adopted now. **Done**, as drafted.

**Catches** `Path::new(".foo")` / `PathBuf::from(".foo")`: resolved against the
working directory, which under `cargo test` is the crate directory, so a test
writes into the source tree. Shepr already guards the production side (`AppPaths`
rejects relative inputs); this covers the literal spelling.

```toml
[[textlint]]
name = "no-relative-dot-directory-path"
pattern = '\b(?:PathBuf::from|Path::new)\(\s*"\.[A-Za-z_]'
paths = ["crates/**/*.rs", "src/**/*.rs"]
message = "a relative dot-directory path resolves against the working directory, which under cargo test is the crate directory; in a test use shepr_test_support::ScratchDir, and for a path only compared or rendered spell it /nonexistent/..."
```

No `region`: the pattern contains a double quote (see A9).

**Violations: 0** (tripwire). The `"./..."` literals in the tree are OpenCode
plugin specs and path-token test inputs, not dot-directories.

## A6. Durable text does not cite `notes/`

**Decision:** adopted now. **Done**, as drafted.

**Catches** a code comment, manifest comment or root convention document that
sends a reader into `notes/`. Stated verbatim in `AGENTS.md`'s document-folder
section. Broadarrow does this in `check_doc_conventions.py`; for shepr a textlint
is enough, since shepr has no `docs/` or `reference/` yet.

```toml
[[textlint]]
name = "durable-text-does-not-cite-notes"
pattern = '(?:^|[^\w/.-])notes/'
paths = ["crates/**", "src/**", "build.rs", "Cargo.toml", "clippy.toml", "LLM.md", "docs/**", "reference/**"]
allow_marker = "doc-conventions-ok"
message = "a durable file may not send its reader into the notes folder, which carries no truth guarantee; carry the context here"
```

`brokkr.toml` is excluded for the same self-match reason as A2. The message
avoids spelling the folder with a slash for the same reason, should anyone widen
`paths` later.

**Violations: 0** (tripwire).

## A7. Cited repository paths exist (script)

**Decision:** adopted now, together with the hand fixes below: the
`research/alacritty` and `research/vte` citations are reworded to point at the
pinned `alacritty_terminal` and `vte` sources in the cargo registry, the way
`AGENTS.md` already does.

**Done**, as `scripts/check_cited_paths.py`, with two widenings: sources also
include `.ts`, `.js` and `.yaml`, and a `*` block-comment continuation line
counts as a comment. The two dangling citations and both `research/` citations
are fixed.

**Catches** a backticked repository path in a comment or root document that no
longer names a file. This is the checkable half of the stale-citation problem: a
path either exists or it does not. Shepr has live instances, both left behind by
the crate extraction.

**Script:** a trimmed port of broadarrow's `check_doc_conventions.py`. The
original is not portable as-is: its `main` refuses when `reference/` or `docs/`
is missing (shepr has neither), and its fourth leg reads `broadarrow`'s AGENTS.md
inventory tables, which shepr does not have. Keep the cited-path leg and the
markdown-link leg; the notes leg moves to A6.

```python
#!/usr/bin/env python3
"""Every backticked repository path in a comment or a durable document exists.

A citation is claimed only when it is anchored in one of this repository's own
top-level directories, and for `crates/` only when the second component is a real
crate, so a dependency's own `crates/...` spelling and a sibling-relative
`pane/foo.rs` are left alone. Suffix matching cannot tell a deleted file from a
renamed one, so unanchored citations are not claimed at all.
"""

import pathlib
import re
import sys

from _brokkr_config import ROOT, repository_sources, skipped_dir

EXEMPT_MARKER = "doc-conventions-ok"
EXEMPT_LOOKBACK = 2
CODE_SPAN_PATH = re.compile(
    r"`([A-Za-z0-9_.][A-Za-z0-9_./-]*/[A-Za-z0-9_.-]+"
    r"\.(?:rs|md|toml|py|sh|ts|js|json|jsonl|yaml))`"
)
MARKDOWN_LINK = re.compile(r"\[[^\]]*\]\(([^)#]+)\)")
UNCHECKED_PREFIXES = ("research/", "target/", "http")
COMMENT_LINE = re.compile(r"^\s*(?:///|//!|//|#)")


def roots() -> set[str]:
    return {p.name for p in ROOT.iterdir() if p.is_dir() and not skipped_dir(p.name)}


def claimed(target: str, anchors: set[str]) -> bool:
    parts = target.split("/")
    if parts[0] not in anchors:
        return False
    return parts[0] != "crates" or (len(parts) > 1 and (ROOT / "crates" / parts[1]).is_dir())


def exempted(lines: list[str], index: int) -> bool:
    return any(EXEMPT_MARKER in line for line in lines[max(0, index - EXEMPT_LOOKBACK) : index + 1])


def main() -> int:
    anchors = roots()
    documents = sorted(p for p in ROOT.iterdir() if p.is_file() and p.suffix == ".md")
    documents += [p for p in repository_sources({".md"}) if p.parts[len(ROOT.parts)] in {"docs", "reference"}]
    sources = repository_sources({".rs", ".toml", ".py", ".sh"})
    if not sources:
        print("no repository sources found - a pass over nothing is not a pass")
        return 1
    violations = []
    for path in documents + sources:
        relative = path.relative_to(ROOT)
        comments_only = path.suffix != ".md"
        lines = path.read_text(errors="replace").splitlines()
        for number, line in enumerate(lines, start=1):
            if comments_only and not COMMENT_LINE.match(line):
                continue
            if exempted(lines, number - 1):
                continue
            for target in CODE_SPAN_PATH.findall(line):
                if target.startswith(UNCHECKED_PREFIXES) or not claimed(target, anchors):
                    continue
                if not (ROOT / target).is_file():
                    violations.append(f"{relative}:{number}: cites {target}, which is not in the tree")
            if not comments_only:
                for target in MARKDOWN_LINK.findall(line):
                    target = target.strip()
                    if target and ":" not in target and not (path.parent / target).exists():
                        violations.append(f"{relative}:{number}: link to {target} does not exist")
    for violation in violations:
        print(violation)
    if violations:
        print(f"{len(violations)} dangling citation(s)")
        return 1
    print(f"checked {len(documents)} document(s) and {len(sources)} source file(s)")
    print("cited paths ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
```

```toml
[[script_check]]
name = "cited-paths"
command = "python3 scripts/check_cited_paths.py"
expect = "cited paths ok"
match = "last-line"
stage = "pre-clippy"
```

The markdown-link leg uses `Path.exists`, which is fine in a script; the clippy
seal in B4 is about Rust.

**Violations (2):**

- `crates/shepr-api/src/client.rs:161` cites `src/api/wait.rs`; the file is
  `crates/shepr-api/src/wait.rs` (`prompt_agent` is there).
- `crates/shepr-api/src/server.rs:38` cites `src/server/alt_screen_read.rs`; the
  file is `crates/shepr-server/src/server/alt_screen_read.rs`.

Not caught (not backticked, or under `research/`), fix by hand in the same pass:
`Cargo.toml:18` says the reference checkout is in `research/alacritty`, and
`crates/shepr-mux/src/pane/osc.rs:19` cites `research/vte/src/lib.rs`. Shepr's
`research/` holds only `broadarrow`; both get reworded to cite the pinned
`alacritty_terminal` and `vte` sources in the cargo registry (decided; also
HYGG-113 in `notes/hygiene-guards.md`).

## A8. `#[allow]` carries a comment saying why

**Decision:** dropped. B9 denies `#[allow]` outright with the reason written
into the `#[expect]`, which supersedes this rule.

**Catches** an `#[allow(...)]` with no comment. `AGENTS.md`: "`#[allow]` only with
a comment saying why." This is the textlint form of the stated rule; B9 is the
stricter compiler form broadarrow uses.

```toml
[[textlint]]
name = "allow-attributes-say-why"
pattern = '#!?\[allow\('
paths = ["crates/**/*.rs", "src/**/*.rs", "build.rs"]
region = "code"
allow_marker = "//"
allow_marker_above = 1
message = "an #[allow] carries a comment saying why, on its own line or the line above"
```

**Violations (1):** `crates/shepr-mux/src/workspace/tab.rs:128` (blank line above).

Known gap: a doc comment above also contains `//`, so
`crates/shepr-mux/src/workspace/tab.rs:210` and `:247`, whose only neighbour is the
function's `///` doc, pass without a reason. B9 closes that.

## A9. Checks on the checks (scripts, portable as-is)

**Decision:** adopted now. B1 and B7 both use `skip_after`, so
`check_skip_after_scopes.py` is wired with B1 at the latest.

**Done**: both scripts are wired now. Two changes from broadarrow's copies:
both resolve `[textlint_preset]` before reading a rule (a preset can carry
`region` or `skip_after`), and the skip-after witness's globs follow globset's
defaults, where `*` crosses `/`, as brokkr's own matcher does. The witness
reports "nothing to witness" until B1 adds the first `skip_after`.

Both keep the textlints above honest. Pure hygiene with no code impact.

**`check_textlint_regions.py`** refuses a `region = "code"` rule whose pattern
contains `"` or `:?`: brokkr masks string literals, quotes included, so such a
rule can never fire. Broadarrow found five inert rules this way. Portable as-is
with the `_brokkr_config.py` port. Current shepr violations: 0 (brokkr.toml has
no textlints yet; none of the drafts here pairs `code` with a quote).

**`check_skip_after_scopes.py`** re-applies every `skip_after` rule to the
production items below a file's first `#[cfg(test)]`. `skip_after` is
line-based, so an early `#[cfg(test)]` item releases every production line under
it. Portable as-is. It matters in shepr specifically, because the tree has early
test-gated items at file tops:

- `crates/shepr-remote/src/lib.rs:3` (`#[cfg(test)] mod attach;`) - a
  `skip_after = '^#\[cfg\(test\)\]'` rule would silently exempt the production
  `eprintln!` calls at `lib.rs:263-277`
- `crates/shepr-mux/src/pane/runtime.rs:30` (a test-only `use`)
- `crates/shepr-agent/src/integration/opencode_config.rs:82` and `:105`

None of the A drafts uses `skip_after`; B1 and B7 do, so wire this script with
the first of them that lands.

```toml
[[script_check]]
name = "textlint-regions"
command = "python3 scripts/check_textlint_regions.py"
expect = "textlint regions ok"
match = "last-line"
stage = "pre-clippy"

[[script_check]]
name = "skip-after-scopes"
command = "python3 scripts/check_skip_after_scopes.py"
expect = "skip_after scopes ok"
match = "last-line"
stage = "pre-clippy"
```

## A10. Headless internal events route through the forwarding method

**Decision:** adopted now; the inert unit test is deleted in the same commit. No
hygiene or bug entry records the inert test, so there is nothing to close
elsewhere.

**Done**: the textlint is in and the inert unit test is deleted.

Not a broadarrow rule, but the broadarrow "has one owner" shape, and it replaces
an existing guard that is inert.

`crates/shepr-server/src/server/headless/tests/mod.rs:6488`,
`no_handle_internal_event_bypass_in_module`, is a hand-rolled textlint in a unit
test. It is blind twice over:

1. It `include_str!`s `headless.rs` and looks for
   `fn handle_internal_event_with_forwarding` there, but the method now lives in
   `headless/internal_events.rs`. A bypass in any `headless/*.rs` submodule is
   invisible to it.
2. It looks for `self.app.handle_internal_event(` and
   `..._with_render_impact(`, but the forwarding method itself calls
   `handle_internal_event_with_pane_updates` and
   `handle_internal_event_with_render_demand`. A bypass through either passes.

```toml
[[textlint]]
name = "headless-internal-events-go-through-forwarding"
pattern = '\.handle_internal_event(?:_with_[a-z_]+)?\s*\('
paths = ["crates/shepr-server/src/server/**/*.rs"]
exclude = [
    "crates/shepr-server/src/server/headless/internal_events.rs",
    "crates/shepr-server/src/server/headless/tests/**",
    "**/tests.rs",
    "**/*_tests.rs",
]
except = ['\bhandle_internal_event_with_forwarding\s*\(']
region = "code"
message = "route an internal event through HeadlessServer::handle_internal_event_with_forwarding, which forwards clipboard writes to the foreground client"
```

**Violations: 0.** The three calls in `headless.rs` (lines 519, 547, 2111) are the
forwarding method and are excepted. Delete the unit test in the same commit.

---

# B: seals that change how code is written

## B1. Print macros

**Decision:** adopted now, in the recommended form: the library-crate textlint
below, leaving `src/` alone. The workspace clippy seal is not adopted, so the
CLI's own `eprintln!` sites (HYGC-004) are not held by this rule. Wire A9's
`check_skip_after_scopes.py` with it, or the three `shepr-remote/src/lib.rs`
prints stay hidden.

**Catches** `print!`/`println!`/`eprint!`/`eprintln!`. They panic on a closed pipe
and reach no log file. In shepr the CLI (`src/`) is safe on the first count:
`shepr_platform::begin_cli_output` resets `SIGPIPE` to default, so a closed pipe
kills the CLI quietly. Library crates are not covered by that. The remote bridge
(`crates/shepr-remote/src/remote/bridge.rs:145, 158`) writes to a stderr that an
SSH session may already have closed, and the headless server is daemonized with
stderr on `/dev/null`, so its prints go nowhere.

**Recommended draft:** a library-crate textlint, leaving `src/` alone.

```toml
[[textlint]]
name = "library-crates-do-not-print"
pattern = '\b(?:print|println|eprint|eprintln)!\s*[\(\[\{]'
paths = ["crates/*/src/**/*.rs"]
exclude = ["**/tests/**", "**/tests.rs", "**/*_tests.rs", "crates/shepr-test-support/**"]
skip_after = '^#\[cfg\(test\)\]'
region = "code"
message = "a print macro panics on a closed pipe and reaches no log file; log through tracing, or return the text to the CLI to print"
```

**Violations (24 lines in 6 files):**

- `crates/shepr-server/src/server/headless/bootstrap.rs:43, 44, 73, 74, 152-161` (9)
- `crates/shepr-remote/src/lib.rs:263, 273, 277` (3; hidden by `skip_after`
  unless A9's witness is wired, see there)
- `crates/shepr-remote/src/remote/bridge.rs:145, 158` (2)
- `crates/shepr-remote/src/remote/server_lifecycle.rs:141, 148-157, 174` (8,
  including the interactive `eprint!` prompt)
- `crates/shepr-agent/src/integration/registry.rs:280` (1)
- `crates/shepr-client/src/lib.rs:237` (1)

The clippy seal form (`disallowed-macros` on `std::print` and friends) would flag
143 lines in 20 files, 109 of them in the CLI (`src/`) and 3 in `build.rs`
(cargo protocol output), and needs an owned output sink in
`src/cli`. Only worth it if the owner wants the CLI routed through one sink too.

## B2. `process::exit` at a named boundary

**Decision:** adopted now: `process::exit` is confined to `src/main.rs`. The
`exits-from-main` textlint below states exactly that; the clippy form would need
an `#[expect]` at each of `main.rs`'s nine sites to say the same. Ties to HYGC-025
and BUG-072. One site is harder than it looks: the `remote_bridge.rs` exit runs
on a watchdog thread precisely because the relay's own thread may be blocked in
a read (its comment: "returning from a blocked copy cannot guarantee shutdown,
and joining it could hang forever"). Moving the exit to `main.rs` needs a way to
unblock that copy (shutting down the fds, or a poll with a deadline), not just a
returned outcome.

**Catches** exits that skip destructors. Shepr has real destructor-dependent
cleanup: `TerminalGuard::drop` restores the host terminal, and
`release_ssh_resources_before_exit` exists because `process::exit` skips it
(`crates/shepr-remote/src/remote/ssh.rs:149`).

```toml
# clippy.toml, appended to disallowed-methods
  { path = "std::process::exit", reason = "return a status to main; expect only at a named process boundary" },
```

Or the lighter textlint form of broadarrow's `cli-exits-from-one-file`:

```toml
[[textlint]]
name = "exits-from-main"
pattern = '\bprocess::exit\s*\('
paths = ["crates/**/*.rs", "src/**/*.rs"]
exclude = ["src/main.rs"]
region = "code"
message = "exit only from src/main.rs; return the status or error instead"
```

**Violations.** Clippy form: 13 in 4 files (`src/main.rs` 9, plus the three
below). Textlint form: 4 in 3 files:

- `crates/shepr-platform/src/remote_bridge.rs:56`
- `crates/shepr-server/src/server/headless/bootstrap.rs:45, 75`
- `crates/shepr-client/src/lib.rs:311` (it does release SSH and flush logs first)

## B3. `debug_assert!` and `debug_assertions`

**Decision:** adopted now, all three pieces. Each `debug_assert!` becomes an
`assert!` where a panic is the right containment, and typed handling otherwise
(BUG-058; `unregister_moved_pane` is already decided as a deletion). The
`cfg!(debug_assertions)` switch in `crates/shepr-config/src/io.rs::app_dir_name`
is removed, so dev and release builds resolve the same config, state and runtime
directories. Consequence to plan for: `app_dir_name` also names the runtime
directory the sockets live in, so after the change a dev build started with
`AGENTS.md`'s `env -u SHEPR_SOCKET_PATH -u SHEPR_CLIENT_SOCKET_PATH brokkr run
-- ...` recipe resolves the installed server's sockets and state rather than
starting its own; that recipe needs a replacement (for example a named session
or a scratch `XDG_*` set) when the switch goes. The `cfg!(test)` arm is a
separate question (BUG-021).

**Catches** checks whose behaviour depends on the profile. Directly relevant to
shepr: `brokkr check` and (with `[test] debug = true`) `brokkr test` build dev,
while `brokkr install` builds release, so the installed binary is a profile no
sweep builds. A `debug_assert!` either panics before the handling below it runs or
vanishes in the shipped binary.

```toml
# clippy.toml
disallowed-macros = [
  { path = "core::debug_assert", reason = "panics in one profile and vanishes in the other; use assert! or handle the case" },
  { path = "core::debug_assert_eq", reason = "panics in one profile and vanishes in the other; use assert_eq! or handle the case" },
  { path = "core::debug_assert_ne", reason = "panics in one profile and vanishes in the other; use assert_ne! or handle the case" },
]
```

```toml
# Cargo.toml [workspace.lints.clippy]; shepr denies only disallowed_methods today
disallowed_macros = "deny"
```

```toml
[[textlint]]
name = "no-profile-dependent-behaviour"
pattern = '\bdebug_assertions\b'
paths = ["crates/**/*.rs", "src/**/*.rs"]
region = "code"
message = "behaviour must not depend on the build profile; the installed build is release and no sweep builds it"
```

**Violations.** Macros: 5 in 3 files:
`crates/shepr-client/src/shell/navigation/actions.rs:338`,
`crates/shepr-client/src/endpoint/activation.rs:987, 988`,
`crates/shepr-server/src/server/headless/lifecycle.rs:89, 300`.
`debug_assertions`: 1, `crates/shepr-config/src/io.rs:23` (`app_dir_name` returns
`shepr-dev` in a dev build and `shepr` in release, so config and state paths differ
by profile). See the surprises section for the stale comment above it.

## B4. `Path::exists`

**Decision:** adopted now: the clippy seal, with `try_exists` or a match on
`NotFound` at each site. Clippy runs over test targets too, so the roughly 40
test-file sites convert as well. The seal is extended to `Path::is_file` and
`Path::is_dir`, which swallow stat errors the same way (HYGG-078's
`ssh_config_include` is one).

**Catches** presence probes that swallow the stat error (`EACCES`, `ELOOP`) as
"absent". Relevant to shepr's config and state paths, where a permission problem
should fail the launch ("Any config problem fails the launch; no fallbacks").

```toml
  { path = "std::path::Path::exists", reason = "preserve stat errors with try_exists or match NotFound" },
```

**Violations: about 106 call sites in 24 files** (matched on `.exists()`, so a few
may be other types). About 40 are in test files
(`crates/shepr-agent/src/integration/tests.rs` 33,
`crates/shepr-agent/src/integration/config_file/tests.rs` 3,
`crates/shepr-platform/src/tests.rs` 1, `src/test_support.rs` 1,
`crates/shepr-test-support/src/lib.rs` 2), and most of
`crates/shepr-mux/src/persist/writer.rs`'s 19 are in its test module, as are all 5
in `crates/shepr-remote/src/remote/attach.rs` (a test-only module). Production
concentrations: `crates/shepr-mux/src/persist/io.rs` 11,
`crates/shepr-server/src/app/tab_bar_status.rs` 5,
`crates/shepr-agent/src/integration/registry.rs` 4. Full per-file list: rerun
`rg -c '\.exists\(\)' crates src build.rs`.

## B5. `catch_unwind` has one owner

**Decision:** adopted now. The scoped helper comes first; `shepr-core` is the
home every site's crate can reach. No hygiene or bug entry records the five
sites.

**Catches** ad hoc panic containment. Broadarrow routes every catch through one
scoped helper. Shepr has five independent sites:

- `crates/shepr-vt/src/locks.rs:63`
- `crates/shepr-api/src/event_hub.rs:134`
- `crates/shepr-server/src/app/git_refresh.rs:250`
- `crates/shepr-pty/src/actor.rs:913`
- `crates/shepr-mux/src/persist/writer.rs:971` (test module)

```toml
  { path = "std::panic::catch_unwind", reason = "contain panics through the one scoped helper; expect at its owner" },
```

Adopting it means writing that helper first (in `shepr-core` or `shepr-platform`,
per the layering). Low value at five sites unless the owner wants a named
contract for what a caught panic does.

## B6. Every child gets a stated working directory

**Decision:** adopted now, in the clippy form (clap's `Command::new` would trip a
textlint). Tests spawn through one helper that sets a scratch working directory;
most remaining test spawns are host programs that piece 3 of the test-isolation
work removes anyway, so the helper mostly serves the re-exec and stand-in
spawns. `build_server_daemon_command` becomes a violation and gets a stated
directory. The seal is about children only: `std::env::current_dir()` read as a
test input (HYGP-005, HYGG-010) is a different spelling it does not catch.

**Catches** `Command::new` without `current_dir`: the child inherits the parent's
directory. For shepr this has a concrete consequence:
`crates/shepr-remote/src/remote/local_server.rs:127-151`,
`build_server_daemon_command`, does not set `current_dir`, so the long-lived
server daemon keeps the launching shell's directory as its working directory for
its whole life (the startup cwd is passed separately through an environment
variable). That pins the directory: an unmount fails with `EBUSY`, a deleted
directory stays referenced, and any relative path the server ever resolves lands
there. Possibly intentional; worth one decision.

```toml
  { path = "std::process::Command::new", reason = "a child gets a stated working directory; expect at a site naming it" },
```

Only the `std` path is needed: the one tokio use
(`crates/shepr-server/src/app/tab_bar_status.rs:550, 935`) is
`tokio::process::Command::from(std_command)`. A `tokio::process::Command::new`
entry would also be unresolvable in crates without tokio (`shepr-core`,
`shepr-vt`, `shepr-test-support`), which broadarrow's own `clippy.toml` says makes
clippy error out.

**Violations: about 42 in 24 files**, excluding clap's `Command::new` in
`src/cli/spec.rs` (63) and `src/cli/spec/machine.rs` (5), which the path-based
seal does not touch. Largest: `crates/shepr-agent/src/integration/tests.rs` 4,
`crates/shepr-remote/src/remote/attach.rs` 3, `crates/shepr-mux/src/git/status.rs`
3, `crates/shepr-platform/src/tests.rs` 3 (`attach.rs` is test-only, see the
surprises). Four sites already set `current_dir` on the child
(`crates/shepr-pty/src/command.rs:155`, `tab_bar_status.rs:546`,
`crates/shepr-agent/src/detect/mod.rs:1358`, `crates/shepr-mux/src/pane/runtime.rs:2132`). Clap's name clash is why this must be the clippy form, not a
textlint.

## B7. Clock seam for `AppState`

**Decision:** adopted incrementally, as part of the hygiene work: time is passed
in rather than `Instant::now()` / `SystemTime::now()` read inside logic, and each
subsystem that gets its seam is held by a scoped textlint in the shape of
broadarrow's `control-loop-reads-the-clock-seam` (the draft below is the
`shepr-server/src/app/` instance). The tree-wide finding is HYGP-001.

**Catches** wall-clock reads in state code. `AGENTS.md` says `AppState` is pure
data, testable without PTYs or async; it reads `Instant::now()` directly in many
places, so time-dependent behaviour (resume windows, git refresh cadence,
double-click timing) cannot be driven by a test clock.

```toml
[[textlint]]
name = "app-state-reads-the-clock-seam"
pattern = '\b(?:Instant|SystemTime)::now\s*\('
paths = ["crates/shepr-server/src/app/**/*.rs"]
exclude = ["**/tests.rs", "**/tests/**", "**/*_tests.rs"]
skip_after = '^#\[cfg\(test\)\]'
region = "code"
message = "AppState reads time through its clock seam so tests can move it"
```

**Violations: about 70 in 13 files** under `crates/shepr-server/src/app/`
(`agent_resume.rs` 19, `git_refresh.rs` 10, `mod.rs` 10, `session.rs` 9,
`api/agents.rs` 6, `tab_bar_status.rs` 5, and smaller). Tree-wide there are about
600 clock reads. This is a refactor, not a lint, which is why it lands scope by
scope.

## B8. Per-crate limits modules

**Decision:** adopted incrementally, as part of the hygiene work: each crate gets
a `limits` module, and once a crate's constants have moved, textlints in the
shape of `numeric-consts-live-in-limits` and
`duration-and-capacity-literals-live-in-limits`, scoped to that crate, hold it.
The tree-wide finding is HYGV-036.

Broadarrow's `numeric-consts-live-in-limits` and
`duration-and-capacity-literals-live-in-limits` force every numeric or `Duration`
const into a `limits` module. Shepr has one such module already
(`crates/shepr-protocol/src/limits.rs`) and about 370 numeric/`Duration` consts
in about 95 files, which is why this goes crate by crate. The server and client
event loops, where timeouts are actually tunable, are the natural first
crates.

## B9. `#[allow]` becomes `#[expect(..., reason = ...)]`

**Decision:** adopted now, including the `cfg_attr` form for the `dead_code`
cases. It supersedes A8, and it changes the rule `AGENTS.md` states ("`#[allow]`
only with a comment saying why"), which needs rewording when this lands.

Broadarrow denies `allow_attributes` and `allow_attributes_without_reason`: an
`#[expect]` fires when the suppression stops being needed, and the reason is
written into the attribute. Shepr's env-seal work already uses the
`#[expect(clippy::disallowed_methods, reason = ..)]` idiom, so this would make
that the only form.

```toml
# Cargo.toml [workspace.lints.clippy]
allow_attributes = "deny"
allow_attributes_without_reason = "deny"
```

**Violations: 40 `#[allow]` sites in 20 files** (full list:
`rg -n '#!?\[allow\(' crates src`). Most are `too_many_arguments` (21, in
`crates/shepr-mux/src/workspace.rs`, `workspace/tab.rs`, `pane/runtime.rs`,
`crates/shepr-client/src/lib.rs`, `input.rs`, `shell_runtime.rs`) and cast lints
(10); the rest are `dead_code` (3), `result_unit_err` (2), `unwrap_in_result`
(2), `large_enum_variant` (1) and `redundant_closure_for_method_calls` (1). Caveat for the three `dead_code` allows
(`crates/shepr-vt/src/modes.rs:69, 77`, `crates/shepr-mux/src/pane/terminal.rs:575`):
the items are used by tests, so `#[expect(dead_code)]` would be unfulfilled in
test builds; they need `#[cfg_attr(not(test), expect(dead_code, reason = ...))]`.
If adopted, A8 becomes redundant.

## B10. Extra compiler lints and the rustdoc phase

**Decision:** adopted; applied in a later round.

Broadarrow denies lints shepr does not: `[workspace.lints.rust]`
(`unreachable_pub`, `rust_2018_idioms`, `single_use_lifetimes`, `trivial_casts`,
`trivial_numeric_casts`, `unused`, `variant_size_differences`,
`non_ascii_idents`, `future_incompatible`), extra clippy lints
(`let_underscore_must_use`, `match_same_arms`, `redundant_else`,
`unnested_or_patterns`, `map_unwrap_or`), and `[workspace.lints.rustdoc]`
(`broken_intra_doc_links` and five more) evaluated by brokkr's `[rustdoc]` phase:

```toml
# brokkr.toml
[rustdoc]
document_private_items = true
```

Shepr's doc comments use intra-doc links (for example
``[`posix_shell_command`]`` in `crates/shepr-remote/src/remote/launch.rs`), and
nothing checks them today. Violation counts need a clippy or rustdoc run, which
this pass did not do. `brokkr man config rustdoc` notes that
`private_intra_doc_links` dominates such a run; allow it in
`[workspace.lints.rustdoc]` if private items are documented.

Not needed: a doctest sweep. Shepr has no Rust doctests (its three fenced blocks
are `toml` and `text`).

## B11. Release-profile sweep

**Decision:** rejected. `brokkr check` does not build the release profile, in any
form; the owner's choice. The draft stays below only so it is not re-proposed.
Instead, the root `Cargo.toml` release profile sets `overflow-checks = true`, so
integer overflow panics in the installed build as it does in the gated one.
With `debug_assert!` and `cfg!(debug_assertions)` gone (B3), integer overflow
(panics in dev, wraps in release) is the main behaviour left that differs
between the gated and the installed build.

**Catches** behaviour that differs between the tested dev build and the installed
release build: integer overflow (panics in dev, wraps in release) and
`cfg!(debug_assertions)`. Shepr has no `[bin] debug`, so `brokkr install` ships
release, and nothing builds release in `brokkr check`.

```toml
[[check]]
name = "release-profile"
packages = ["shepr-core", "shepr-protocol", "shepr-vt"]
profile = "release"
only = ["overflow", "clamp", "wrapping", "truncat"]
curated = true

[test]
debug = true
default_profile = "standard"

[test.profiles.standard]
description = "The edit-time workspace sweep"
sweeps = ["workspace"]

[test.profiles.release]
description = "Profile-sensitive tests, built the way brokkr install builds"
sweeps = ["release-profile"]
```

Without the two profiles, bare `brokkr check` would run the release sweep every
time. With them it runs as `brokkr check --profile release`. The filters match
current tests including
`pane_id_allocation_stops_at_the_end_of_the_id_space_instead_of_wrapping`,
`text_area_pixel_report_does_not_overflow_for_large_cells`,
`varint_rejects_overlong_overflow_and_truncation` and
`client_surface_clamp_fits_server_geometry_limit`. Violations: unknown until run.

## B12. Install shape (native brokkr)

**Decision:** tracked elsewhere: it is the shipped-feature-set gate check of
piece 4 of the test-isolation work, recorded under BUG-057 (`notes/bugs.md`)
and HYGP-031 (`notes/hygiene-policy.md`). Not added separately from here.

Broadarrow checks the `cargo install` feature graph with a package-unified sweep
plus two scripts. Shepr's brokkr does it natively:

```toml
[bin]
install = ["shepr"]
install_feature_check = "always"
```

This compiles `shepr` the way `cargo install` resolves it, without features that
sibling members or dev-dependencies donate. It matters for shepr because the root
package's dev-dependencies turn on `test-support` / `test-api` features of seven
crates, which workspace unification can leak into what `brokkr check` compiles.
Violations: unknown until run.

## B13. Tree debris (script)

**Decision:** adopted; applied in a later round.

`check_tree_debris.py` refuses anything in a crate root outside
`Cargo.toml`, `README.md`, `build.rs`, `src`, `tests`, `benches`, `examples`, an
untracked file under a crate's sources with an extension the tracked sources do
not use, and any root entry neither tracked nor gitignored. It is the backstop
for a test writing through a relative path. Portable with one edit: `REMEDY`
names `broadarrow_test_support`; point it at `shepr_test_support::ScratchDir`.

**Violations: 0** today (every crate root is `Cargo.toml` + `src`, plus
`crates/shepr-protocol/build.rs`). Close to the test-isolation work; could go with
it.

## B14. Scripts roster (script)

**Decision:** adopted; applied in a later round. The roster now also has
`_brokkr_config.py`, the three gate scripts wired by A7 and A9, and the
`textlint_sweep.py` diagnostic to classify.

`check_scripts_roster.py` requires a `scripts/README.md` table stating each
script's standing (gate, tool, diagnostic) and that every `gate` is wired in
`brokkr.toml`. Portable as-is; `SHARED_GATE_FILES` becomes `{"_brokkr_config.py"}`.
**Violations:** every existing script lacks a row: `scripts/fix_unwraps.py`,
`scripts/notes_drop.py`, and the untracked `scripts/hygiene_merge.py`. Worth it
once there are three or more wired script checks; it also forces a decision on
whether the one-shot migration scripts should stay.

## B15. Workspace dependency pins (script)

**Decision:** adopted; applied in a later round.

`check_workspace_dependencies.py`: every name in the root
`[workspace.dependencies]` is taken with `workspace = true` and no restated
version, and an external dependency used by two or more members must be pinned
there. Portable with the `_brokkr_config.py` glob fix above. **Violations: 0**:
every member already inherits. A cheap tripwire if the owner wants it.

## B16. Tests do not read compiled content

**Decision:** adopted; applied in a later round. The first violation below
disappears with A10's test deletion.

Broadarrow's `no-test-reads-compiled-content` refuses `include_str!` in tests (read
from disk through `CARGO_MANIFEST_DIR` instead). Shepr bundles manifests, the
default config and agent assets with `include_str!` in production, which is
correct. Test uses:

- `crates/shepr-server/src/server/headless/tests/mod.rs:6489` (the inert guard
  A10 replaces)
- `crates/shepr-termio/src/input/raw_input.rs:2328, 2334` and
  `crates/shepr-termio/src/input/parse.rs:1157, 1163`, each climbing
  `../../../../tests/fixtures/` from a library crate into the root package's
  fixture directory

```toml
[[textlint]]
name = "no-test-reads-compiled-content"
pattern = '\binclude_(?:str|bytes)!'
paths = ["crates/**/*.rs", "src/**/*.rs"]
exclude = [
    "crates/shepr-config/src/lib.rs",
    "crates/shepr-agent/src/detect/manifest.rs",
    "crates/shepr-agent/src/integration/mod.rs",
]
region = "code"
message = "a test reads its fixture from disk through CARGO_MANIFEST_DIR; include_str! is for content shepr ships"
```

**Violations: 5 in 3 files** (above). Mostly worth it for the cross-crate
`tests/fixtures` climb, which breaks silently if either crate moves.

## B17. Seal witness and a single clippy.toml

**Decision:** adopted (a witness that `clippy.toml`'s ban paths still resolve);
applied in a later round. The draft below argued against it; the owner
decided otherwise.

Broadarrow's `check_origin_seal.py` proves each `disallowed-methods` path still
binds, by linting a fixture crate. Mostly Nautilus-specific and heavy (it runs
`cargo clippy` itself). Shepr's seals name only `std` paths, which do not move,
so the witness is not worth porting. Its cheap leg is: the root `clippy.toml` is
the only one (a nested one shadows the root for every crate below it). That is a
few lines of Python, or simply a rule to remember. Not recommended now.

## Not ported, and why

- `textlint-probes` and `textlint-owned-values`: infrastructure for a large rule
  set (broadarrow has over 100 rules). Revisit if shepr's set grows past a few
  dozen.
- `no-systemd-reasoning`: inverted for shepr, which legitimately talks to logind
  (`zbus`, the shutdown inhibitor).
- `spdx-headers`, `licenses`, `add-spdx.py`: shepr is Apache-2.0 with no per-file
  headers; brokkr also has a native `[header]` phase if that changes.
- `package-sweep-shapes`, `install-feature-boundary`, `build-stamp-siblings`,
  `adapter-group`, `ba-verbs`, `run_spec_gates.py`, every venue, Nautilus, Pine,
  daemon and worker rule: broadarrow domain.
- The one-owner value rules (`*-spelled-once`, `*-has-one-owner`): the mechanism
  is generic but every instance is a broadarrow value. Shepr's candidates are
  the "defined at N sites" findings in `notes/hygiene-values.md`; each one, once
  collapsed to one owner, is exactly what such a textlint pins. Most are
  environment variables, which the env seal covers.

---

# Surprises

## In shepr

1. **The internal-event bypass guard is inert** (A10): it scans a file the
   guarded method moved out of, for method names the forwarding method no longer
   calls.
2. **Pre-deployment compatibility code exists** despite `AGENTS.md`:
   `crates/shepr-mux/src/persist/snapshot.rs:201` (older-build `lines` field),
   `crates/shepr-platform/src/logging.rs:557-561` (tightens older-build logs),
   `crates/shepr-client/src/shell/overlays/preferences.rs:129`
   (`legacy_preferences_ignore_unknown_fields`).
3. **Stale profile claims.** `crates/shepr-config/src/io.rs:14-20` says "`brokkr
   test` builds release", and `AGENTS.md`'s build table says `brokkr test` is
   "release profile by default (`--debug` for dev)". `brokkr.toml` sets
   `[test] debug = true`, which per `brokkr man config test` makes `brokkr test`
   build dev. Also, `cfg!(test)` in `app_dir_name` is true only for
   `shepr-config`'s own unit tests; another crate's tests linking `shepr-config`
   get `shepr-dev`, not `shepr-test`, so the comment's guarantee is narrower than
   it reads.
4. **Two TypeScript test suites nothing runs:**
   `crates/shepr-agent/src/integration/assets/shepr-agent-state.test.ts` and
   `.../assets/opencode/shepr-agent-state.test.ts`. No `brokkr.toml` entry or Rust
   test invokes them, so they read as coverage and are not.
5. **Tests that pass by skipping:**
   `crates/shepr-agent/src/integration/tests.rs:3092, 3181` print "skipping:
   python3 is not installed" and return green. Relevant to the host-program work
   under way elsewhere.
6. **A 1112-line module compiled only for tests:** `crates/shepr-remote/src/lib.rs:3-5`
   is `#[cfg(test)] #[path = "remote/attach.rs"] mod attach;`. Either it is dead
   production code kept alive by its own tests, or a test harness that looks like
   production (it has `SAFETY` blocks, `Command::new`, fd handling).
7. **The server daemon inherits the launching shell's working directory** (B6,
   adopted: it becomes a seal violation and gets a stated directory).
8. **Dangling citations** (A7): `src/api/wait.rs`, `src/server/alt_screen_read.rs`,
   `research/alacritty`, `research/vte/src/lib.rs`. Decided: the two `research/`
   citations are reworded to point at the pinned sources in the cargo registry.
9. **`AGENTS.md` describes `reference/` and `docs/`**, but neither exists yet.
   Both are coming soon; nothing to decide.
10. **Out of scope, stays as it is (decided).** `LLM.md`'s clean-room claim
    ("the development agent never sees third-party
    source code") sits beside a hard fork of herdr, an `AGENTS.md` instruction to
    read the pinned `alacritty_terminal` and `vte` sources, and comments citing
    ghostty and vte source files (`crates/shepr-mux/src/pane/terminal/tests.rs:2547`,
    `crates/shepr-mux/src/pane/osc.rs:19`). Worth the owner's attention, since the
    document exists to make a legal-risk statement.
11. **`clippy.toml` cites a brokkr textlint that is not in `brokkr.toml` yet**
    (`disallowed-escapes-are-allowlisted`). Presumably it lands with the env-seal
    work; if that work lands without it, the comment dangles.
12. **Upstream herdr issue numbers** in eight comments (A4).

## In broadarrow

1. **Two contradictory claims about clippy and unresolvable paths.**
   `clippy.toml`'s comment above `disallowed-macros` says clippy "hard-errors
   loading a `disallowed-methods` path it cannot resolve", which is why the async
   `catch_unwind` seal is a textlint. `scripts/check_origin_seal.py`'s docstring
   says `disallowed_methods` "fails open: a configured path that no longer
   resolves ... seals nothing and says nothing about it", which is the whole
   reason that witness exists. At most one is true for the pinned toolchain.
2. **`except_above` and `only_if_file_matches_above` are undocumented** in
   `brokkr man config`, though broadarrow depends on them and brokkr implements
   them.
3. **`check_doc_conventions.py` fails closed on a missing `reference/` or
   `docs/`**, so it cannot be copied to a repository that has neither; the
   refusal is intended, but it is why A7 is a trimmed port.
