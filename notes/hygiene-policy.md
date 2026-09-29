# Hygiene: policy invented per call site, and code that is no longer load-bearing

This file consolidates the findings of the nine-scope hygiene hunt for two of the
eight questions the hunters were asked: question 7 (one rule implemented
independently wherever it was needed, ambient dependencies reached from logic,
shared mutable state whose safety rests on call order, unbounded resources,
secrets in diagnostics, test-only shortcuts production can reach) and question 8
(modules, functions, flags and configuration keys that are no longer
load-bearing). Findings about duplicated or unfindable values, output channels
and error handling, and tests, guards and stale claims are filed in sibling
documents; live defects are in `notes/bugs.md`. This is a working document
assembled from reading, not from running anything: entries may be wrong, and a
later fix pass is expected to find phantoms. Where two hunters read the same
thing differently, or where a hunter marked a claim as an unverified inference,
the entry says so.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

---

## HYGP-154 - The injected events.wait clock has no test that injects it

`shepr-api/src/wait.rs::wait_for_event` takes a clock so its deadline is
testable, but only the real socket path calls it, with `Instant::now`. Add a
test that drives the deadline with a fake clock, or the seam proves nothing.

## HYGP-153 - Two server files still open with an early test-only item

`shepr-server/src/server/alt_screen_read.rs` has `#[cfg(test)]` on a `use` at
the top and `shepr-server/src/lib.rs` has one on an early line, so every
`skip_after` textlint (including the new server transport clock rule) skips
the whole file. Neither has a production violation today; move the test-only
items into the trailing test module so the rules see the production part.

## HYGP-152 - Concurrent bridges for one saved machine may collide on one socket path

The saved-machine bridge socket is named by profile only
(`shepr-ssh-<profile>.sock`), and so is the CLI's `--machine` API bridge
(`shepr-api-ssh-<profile>.sock`), both in the shared runtime directory. Two
clients, or two concurrent `--machine` commands, for the same machine would
hit `AddrInUse`, which is classified as a link failure, so the second may retry
until it gives up. Unconfirmed whether something else keeps them apart; verify
with two clients attached to one saved machine, and if they collide, add a
per-client component to the name.
