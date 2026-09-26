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

## EP-021 - Slow links may never finish full discovery within an attempt

- Saved-machine attempts are capped at 25 s (`ATTEMPT_BUDGET`). On a very slow link with no ControlMaster and no cached metadata, full discovery (three commands plus a status probe per candidate, each a cold connect) could exceed that every time and never connect. `shepr machine add` normally seeds the metadata cache, which avoids discovery.
