# Bugs: client presentation and input (shepr-client shell and input, shepr-termio)

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.

Filed from the client presentation and input hunt. The raw report is in commit
6dc81572 (`notes/hunt-client-presentation.md`).

## CUI-003 - Keys typed before the host applies a report-all push are still stamped report-all

Residue. Leases are now taken only for keys whose host keyboard mode promises a
release, and the mode travels with each input event as a snapshot from the
stdin reader. But the snapshot is taken when the reader frames the bytes, from
the mode shepr last wrote. Keys typed after shepr wrote the report-all push and
before the host applied it are still stamped report-all, so a text press in
that window takes a lease whose release never comes, and focus loss sends a
stale release for it.

Closing it needs the host's acknowledgement of the mode change, or reading
report-all from the bytes themselves (a text key that arrives as plain UTF-8
was not reported-all).
