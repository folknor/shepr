# shepr-usage: agreed design

The working plan for the usage tracker crate, agreed with codex over several
spar rounds. It covers fetching only; the wire projection and the client
display come later. notes/usage.md holds the research digest it started from.

## Boundaries

- **shepr-usage:**
  - linked only by shepr-server, through a dependency rule
  - allowed dependencies: serde (typed credential parsing, so refresh tokens
    are skipped unallocated), serde_json, sha2, base64, shepr-platform,
    tracing
  - knows nothing of panes, workspaces or clients
  - receives resolved sources and the active flag, and publishes snapshots
  - modules: model, source, credentials, reader, transport, claude, codex,
    identity, schedule, worker, registry, limits
- **The server:** owns the worker handle and holds the latest observations as
  plain state. The worker's runtime and all secrets stay outside that state.
- **The agent-environment discovery task:** server wiring. The evidence it
  produces is plain data; it is never saved in the session.

## Provider endpoints

- **Claude usage:** `GET https://api.anthropic.com/api/oauth/usage`, with
  `Authorization: Bearer` and `anthropic-beta: oauth-2025-04-20`.
  - `limits[]` comes first.
  - The legacy `five_hour` and `seven_day` blocks fill in only the windows
    that `limits[]` lacks, each window separately.
- **Claude identity and plan:** `GET https://api.anthropic.com/api/oauth/profile`.
  - Called for identity bootstrap whenever a new generation appears, using
    the same captured token.
  - The routine metadata refresh runs at most hourly. It is separate from
    identity bootstrap.
- **Codex usage:** `GET https://chatgpt.com/backend-api/wham/usage`, with
  `Authorization: Bearer` and `ChatGPT-Account-Id`.
  - Adds `X-OpenAI-Fedramp: true` when the credential's FedRAMP flag is set.

## Transport

- The system `curl` binary as a subprocess. Nothing TLS is compiled into
  shepr: no rustls, ring or aws-lc, and no native-tls either.
- The absolute curl path is resolved once, off the scheduler thread.
  - An executable in a location writable by other users is refused.
  - A bounded capability probe runs without credentials, under the same
    supervision.
  - A missing or unusable curl is a visible transport failure.
- A response whose content encoding is not identity is rejected, whatever
  `Accept-Encoding` asked for.
- A 200 measurement requires the transfer to have completed successfully,
  not just a body that looks like complete JSON.
- curl's environment is cleared. `-q` goes first, so no curlrc is read.
- Request details go through `--config -` on stdin: the URL from a fixed
  table, the Authorization header, the account id header and the
  User-Agent `shepr/<version>`.
  - Values are quoted, with backslash and double quote escaped.
  - A credential value containing CR, LF, NUL or another control character
    is refused locally, before anything is sent.
- Options: `--proto =https`, `--tlsv1.2`, no `-L`, `--noproxy` `*`,
  `--max-time`, `-H Accept-Encoding: identity`, `-sS`, and no retries.
- Each request's total budget is about 10 s. curl's `--max-time` and the
  runner's own deadline are both set from it.
- Each response part goes to its own pipe:
  - headers through `--dump-header /proc/self/fd/N`
  - the body on stdout
  - diagnostics on stderr
- The header parser:
  - picks the final response block, skipping 1xx blocks
  - accepts HTTP/1.x and HTTP/2 status lines, with no reason phrase required
  - keeps trailers separate
  - bounds line length, block count and total bytes
- HTTP evidence is kept even when the transfer fails. A complete 429 header
  block with Retry-After survives a body timeout. A 200 counts only after a
  complete, bounded, valid body.
- How statuses are classified:
  - 401 rejects this credential generation.
  - 403 is access refused, never reported as "expired".
  - 429 means throttled; the credentials stay usable.
  - Curl exit codes, oversize, framing errors, a spawn failure and runner
    exhaustion each stay a distinct class.

## Supervised child runner (shepr-platform)

One runner, used by shepr-git and shepr-usage. Git's private runner goes away.

- **Capacity:** reserved before spawn from an opaque budget handle that
  belongs to the subsystem. Each subsystem's budget is a partition reserved
  for it alone, so Git cannot use up usage's capacity and usage cannot use up
  Git's. A reservation is released only once its attempt has met every
  ownership obligation:
  - Either spawn failed and left no child,
  - or the child is cleaned up and every bounded stream task has finished.
  - A stuck spawn or a pending reap keeps the reservation.
- **Deadline:** absolute and sampled before spawn. Spawn itself cannot be
  interrupted. What the runner guarantees is that the caller stops waiting by
  the deadline and the attempt is invalidated. Spawning, running or reaping
  work that is stuck stays counted against admission.
- **Cancellation and shutdown:** never join a stalled runner or reader
  indefinitely. A completion that arrives after its logical timeout cannot
  revive the attempt.
- **Spawn:** the child is put in its own process group before exec.
- **Stdin:** written with nonblocking writes polled against the same deadline.
- **Output streams:** each has its own cap and is drained concurrently. A
  stream reports its captured prefix, whether it overflowed, whether it
  reached EOF, and any read error. A stream can be set to request termination
  when it overflows.
- **Deadline:** checked even while data keeps arriving. When the deadline
  passes, or a drain fails after the leader has exited, the group is killed.
  The group is not signalled again once its identity is no longer protected.
- **Reaping:** one shared reaper. The result keeps the stream reports even on
  timeout or failure.
- **Ownership:** exactly one owner is responsible for stopping and reaping
  the child, whatever fails: thread creation, a writer failure, a drain panic.
- **Scope:** the runner supervises cooperative host utilities. It does not
  claim to contain descendants that leave the group.
- **Git:** keeps starting from `/` with `-C`, and still treats truncated
  output as a failure.

## Agent environment evidence

- **shepr-detect:** recognition can return the selected process: its pid,
  the start-time identity captured in the same stat read as its name and
  state, and the recognition evidence (agent, name, argv). The detector
  itself is unchanged.
- **Independent recognition in discovery:** the server's discovery task
  recognizes processes on its own, inside its capped boundary.
  - **When it scans:** every 60 s while active, and at once when a pane's
    agent identity changes. The pane's reported identity is only a
    scheduling hint.
  - **Before scanning:** it validates the pane shell's incarnation.
  - **Selecting the job:** the same path as the detector: the foreground
    group's leader job first, then the shell's foreground job.
  - **The provider:** taken from the fresh recognition. A mismatch with the
    pane's hint counts as uncertain and is retried.
  - **Accepted gaps:**
    - a short-lived agent instance between scans
    - suspended or backgrounded agents
    Remembered sources and the server's own defaults cover them.
- **shepr-platform environ reader:** the caller passes the allowlist; the
  names come from the shepr-agent descriptors (`config_dir_override`, plus
  `HOME`). The reader:
  - checks the start-time identity, the mount namespace and the root before
    and after the read. Namespace and root are compared by kernel object
    identity.
  - reads to EOF under a byte cap, so truncation is an error.
  - rejects malformed NUL framing and duplicate allowlisted names.
  - returns raw bytes.
- **Re-recognition:** after the read, the process is recognized again from a
  fresh `/proc` read. Changed recognition evidence makes the result uncertain.
- **Abandoned reads:** a byte cap does not bound how long a `/proc` read
  takes. A discovery read that times out stays counted. A process never gets
  an overlapping replacement read, and a late result is discarded.
- **Evidence:** best effort, not an atomic snapshot, and its type says so.
- **Where it runs:** a separate capped discovery task in the server, never on
  the detection tick. Results are accepted against the pane, runtime and
  process evidence.
- **Refresh:** immediately when the pid, start time or recognition
  fingerprint changes, and periodically otherwise. Uncertain reads are
  retried with backoff.
- **Resolution, per provider:**
  - An absolute override is the source.
  - An absent override means the default under the agent's HOME.
  - An empty `CODEX_HOME` counts as unset.
  - Each of these leaves the source unresolved, never a fallback:
    - a relative override
    - an empty Claude override
    - a missing or invalid HOME when the default is needed
    - a truncated or unreadable environment
    - a foreign namespace or root
- **Other sources:**
  - the server's own `CLAUDE_CONFIG_DIR` and `CODEX_HOME`, resolved by the
    same per-provider rules
  - the ordinary default directories
  - remembered sources

## Identities

Three separate identities:

- **Source:** the provider plus the path as discovered.
- **Credential generation:** a versioned, tagged digest of the fields that
  affect routing and authentication.
- **Quota account:**
  - Claude: provider, account uuid and organization uuid from `/profile`,
    called with the same captured token as `/usage`.
  - Codex: provider, principal and selected account id, merged only when the
    access token's claims bind to the id token. Otherwise it stays unresolved.
  - Email is only ever a label.

Identity is resolved for each new credential generation before its usage is
merged into an account. Metadata changes are reconciled separately from the
generation digest.

Completions are checked against what they captured when they started:

- **A credential read:** checked against its source incarnation, its read
  attempt and the reconciliation epoch. The generation is derived from what
  it read.
- **An HTTP attempt:** captures the credential generation, routing identity
  and account binding, and is checked against them on completion.

- A result from an old generation never updates the replacement source's
  binding, credential availability or health. Its measurement can stay
  attributed to its original account as history.
- Rejecting one generation never suspends another usable generation that
  supplies the same account.

Directory aliases:

- The discovered path is kept, and the current credential candidates of
  aliases are deduplicated.
- Symlink retargeting is observed. The source locator is never permanently
  replaced by its canonical path.

## Credentials

- **shepr never refreshes a token, logs in, or writes any agent file.**
- An expired credential stops supplying new requests. Expiry comes from
  Claude `expiresAt` and from the Codex access token's `exp`. An unknown
  expiry stays unknown.
- Sources are re-read every 30 s of active time, with a bounded read on a
  capped reader thread that carries an attempt id. Late results are
  discarded.
  - A reader that times out keeps its capacity until it finishes.
  - No replacement read starts for a source while its previous reader is
    alive.
  - Running out of reader capacity is reported as exhaustion, not as the
    source being unreadable.
- **A failed read:**
  - stops new requests at once
  - keeps the binding and the history for a grace defined in elapsed active
    time
  - after the grace, marks the source unreadable
- **A valid parse showing logout or API-key mode** invalidates the source
  immediately.
- **A rejected generation** stays suspended until its digest changes.
- **Secrets:** confined to the usage subsystem and redacted. They never
  appear in a snapshot or a persisted record.
- **File checks:**
  - The opened object must be a regular file owned by the server's user.
  - Legitimate symlinks are followed safely.
  - Special files are rejected without blocking on open (`O_NONBLOCK`, then
    `fstat`).
  - Reads are bounded.
  - Remembered paths are revalidated on every use.

## Scheduling

- Gates are keyed by quota account and endpoint. They survive rotation,
  dedupe and pause.
- **Before identity is known:**
  - an unresolved source or generation gets a provisional gate
  - identical credentials are deduplicated before identity is resolved
  - identity bootstrap never bypasses host spacing or its own backoff
- **Host spacing:** at least 5 s between request starts to one host within
  this worker, counting `/profile` requests as well as `/usage` requests.
- **Inactive:** means no new HTTP attempts. The active flag is set while any
  client is attached, including one that only lists this server in its
  sidebar (`has_connection`). The initial value and every transition are sent
  explicitly.
- A request's eligibility is the latest of:
  - its cadence (about 5 min, with jitter that never makes it early)
  - its backoff (429 ladder: 5, 10, 20, 40, then 60 min)
  - Retry-After (seconds or HTTP-date) as a lower bound. A zero or invalid
    value is ignored and the backoff stays.
  - host spacing within this worker
- Monotonic time for scheduling; wall-clock epochs for observations.
- On resume, credentials are reconciled before any overdue HTTP, and missed
  polls are not queued.
- At most one request in flight per host, on short-lived runner threads. A
  curl that hasn't been reaped keeps its host slot, and the host is reported
  as blocked.
- Completions are always delivered. A panic becomes a failure, and cleanup
  ownership is preserved.
- Source sets and the active flag are latest-value and coalesced.

## Observations

- **Snapshot:** carries the worker incarnation and a revision. It is kept as
  shared latest state and woken with `try_send`. The server also reads it
  periodically, whatever the active flag.
- **Per quota account:**
  - key, provider, label, plan
  - measurement: whole-response and atomic, with `observed_at`
  - polling health, per endpoint
  - credential availability, per source
- **Unresolved sources and their diagnostics** appear in the snapshot.
- **Claude limits:**
  - kind
  - model and surface scope
  - a duration derived only for the recognized session and weekly kinds
  - used percent as an Option
  - `resets_at` as an Option
  - the reported applicability flags, such as `is_active`, kept as reported
    because their meaning is unverified
- **Codex groups:**
  - the main `rate_limit` group, and each additional limit (name and
    metered feature)
  - each group with `allowed`, `limit_reached` and primary and secondary
    windows
  - each window with `used_percent`, `limit_window_seconds`, `reset_at` and
    `reset_after_seconds`
  - the account's `rate_limit_reached_type` kept verbatim
- **Not modelled:** credits and spend control. Nothing infers "usable" from
  percentages.
- **Parsing fields:** what the wire requires is kept apart from the
  normalized, optional, validated values. A missing flag never deserializes as
  false, a missing or invalid percent never becomes zero, and an unknown
  verdict string is kept verbatim.
- **`observed_at`:** set when the response is received, not when the server
  reads the snapshot. A successful response replaces the whole measurement and
  withdraws any window it no longer contains. Old windows survive only as
  history, aged separately.
- **Freshness:** a pure interpretation of timestamps at read time.
- **`/profile` success:** never advances usage freshness and never clears a
  usage backoff.

## Persistence

- **Remembered sources:** stored in their own file in the data directory,
  without credentials.
- **Writes:** coalesced and serialized, off the scheduler thread, and their
  failures are reported as a separate health signal.
- **Session state:** not involved.
