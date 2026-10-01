# Technical implementation specification

The single document from which an open TODO item is built to completion without
re-deriving its design. Two implementers working from it independently produce
the same artifact.

## What it is

1. **Every brick.** It lays each step on the road from the current code to the
   finished item. No step is left to discover during implementation.
2. **Obstacles resolved inline.** Anything blocking the road is solved in the
   document, as part of it. An unresolved obstacle is a missing brick.
3. **No deferral.** Nothing in the originating TODO is pushed to "later" -
   deferred work is a hole in the road. (Work that belongs to a genuinely
   separate TODO is named and excluded; that is not deferral.)
4. **No shoehorning.** We do not fit the work into existing abstractions,
   structures, or conventions because they already exist. The structure that
   best serves the end goal is the one we build; whatever stands in its way is
   ripped out and rebuilt. Pre-1.0, breaking any internal API is legal.

5. **The contracts inventoried, then read.** Before surveying any code, inventory
   `docs/` and `reference/` - list them, do not work from memory of what they
   contain - and read whatever you judge relevant to the item, along with
   AGENTS.md, which states much of the project's contract itself. All are
   binding: `docs/` on what the thing does for a user, `reference/` and
   AGENTS.md on how it is built.

   Binding does not mean frozen. A spec may deliberately change a contract - that
   is often the whole point of the work, and rule 4 says as much about the code.
   What it may not do is contradict one silently. Every contract the work changes
   is named in the spec, the change to it is specified as a brick like any other,
   and the document lands updated in the same change; a `reference/` or `docs/`
   file left asserting the old behavior is a defect the same way a stale comment
   is. The failure this rule exists to prevent is the unwitting one - a spec
   authored against a remembered contract, disagreeing with the written one on a
   point nobody noticed, where reviewers reliably find the symptoms without
   finding the cause.

## What it must also pin (or it is aspiration, not a spec)

6. **Verification per brick.** Every change names its gate, matched to what the
   change can break: named `brokkr test` cases for behavior, and a by-hand run
   of the dev build (`brokkr run --debug`) against a local dev server for what
   no test reaches. No gate may run `brokkr install`, run a release build,
   touch the release server or its panes, or connect to an SSH machine; behavior only those would reach
   is pinned by a test instead, built as a brick if none exists. A brick whose load is unproven is not laid. Per
   gate, the spec contains the **exact** command to run - copy-pasteable, flags and
   all, not "run the relevant tests". If no command exists that can verify a gate
   (no path exercises it, no test pins the behavior), building that instrument is
   itself a brick of the spec - specified to the same standard and laid before
   the brick it gates.
   The command list is the minimal set of `brokkr` runs that proves the gates,
   not one run per test. `brokkr check` already runs every test in every sweep,
   so a new or changed test is gated by naming it (the name is what proves it
   exists and was written) and by `brokkr check` passing, never by its own
   `brokkr test` line. A separate run earns its place only when `brokkr check`
   cannot supply that evidence: a by-hand run of the binaries, a script check
   run by hand, an `#[ignore]`d test, or a test that must be seen to fail with
   its production half reverted. List `brokkr check` once per landing, not once
   per brick.
7. **A keep/revert path.** The implementation unit is one coherent, fully
   intrusive change that lands and is then kept or reverted on its gate
   results - never a tiny gated probe or an env-var experiment switch. The
   sequence of such landings is ordered so `brokkr check` stays green at every
   boundary between them. Complete-but-unorderable is a failed spec.
8. **The target as concrete artifacts.** "The ideal structure" is pinned to
   exact types, signatures, ownership, and data flow - buildable, not merely
   directional.
9. **A survey of the ground.** The current structure and everything depending on
   it is inventoried before the teardown, so the rip is precise and drops no
   load-bearing work. Specs authored as a batch reconcile their surveys against
   siblings covering the same ground before any is implemented; a sibling's
   survey may already state the fact that refutes this spec's premise.
10. **A stopping rule.** The rebuild has a bounded blast radius. Where the
    teardown stops, and what is out of scope, is stated explicitly.
11. **The standing references.** Every spec must cite, by path: this document
    (`reference/technical-implementation-spec.md`) as the contract it is
    written against; the document the spec was spawned from (the source naming the
    <!-- doc-conventions-ok: instructs a spec author where their own work item
    lives, rather than depending on that document for anything asserted here -->
    item - a `notes/todo.md` entry, or the `notes/` document the work came out <!-- doc-conventions-ok -->
    of). A spec missing either is incomplete.

## Stance

- **Structural over micro.** The spec pursues the structural change that
  materially moves the goal - real throughput for performance work, real
  capability for feature work - not local tweaks. Full rewrites are labeled
  as such, distinct from local changes.
- **Cleanliness is a deliverable.** No env-var scaffolding, benchmark knobs, or
  temporary routing switches left as the way forward.
- **Unlimited resources, aggressive internal rewrites assumed.** Old
  abstractions earn no protection from age; shared writer abstractions and
  generic reuse are not goals. Correctness and maintainability of the *result*
  still hold.
