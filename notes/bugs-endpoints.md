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

## EP-020 - A hung SSH attempt can outlast the 30-second reconnect promise

- Reconnect backoff is capped at 30 s, so `shepr machine reconnect`'s "retry within 30 seconds" holds, except that an SSH attempt already in flight finishes on its own schedule first; a hung ssh attempt can push past 30 s.
