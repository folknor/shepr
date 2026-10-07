# Usage tracker: methods seen in research/

Goal: show the remaining 5-hour and weekly limits for every Claude and Codex
account shepr detects, on every host.

This is a digest of what the research agents reported about the projects in
`research/`. Nothing here has been verified against live files or endpoints.
Field names, endpoints and behaviour are as each agent read them from that
project's code, and every one of them is an undocumented internal of Claude
Code, Codex, or Anthropic's and OpenAI's backends.

Projects covered:

- `clauth`: a Rust Claude account switcher with a usage monitor. It is very
  large and was written by an LLM fleet.
- `quota`: a multi-provider tracker. It ships as a Tauri desktop app, the
  `quota-core` crate, a CLI and a herdr plugin.
- `herdr-agent-usage`: a Rust herdr plugin, v1.6.3.
- `herdr-agent-usage2`: a Go herdr plugin called "usagebar", v0.5.18.
- `memex`: indexes transcripts and counts tokens. It explicitly does not track
  quota.
- `herdr-disp-model`, `herdr-plus`, `herdr-equalize-panes`: these have nothing
  to do with usage.

## Method A: Claude, the OAuth usage endpoint

Used by clauth and by quota's CLI and desktop app.

The request is `GET https://api.anthropic.com/api/oauth/usage` with these
headers:

- `Authorization: Bearer <accessToken>`
- `anthropic-beta: oauth-2025-04-20`
- clauth also sends `Accept: application/json, text/plain, */*` and
  `Content-Type: application/json`.
- clauth sends `User-Agent: claude-cli/<ver> (external, cli)`, taking `<ver>`
  from `claude --version`. It cites anthropics/claude-code#31637: user agents
  that aren't Claude Code's get throttled much harder. quota sends
  `User-Agent: quota`.

There are two response shapes:

- `limits[]`: clauth treats this as the source of truth.
  - Each entry has `kind`, `percent` (0-100, used), `resets_at` (ISO-8601 with
    an offset) and `scope{model{display_name}, surface}`.
  - `kind` is `session` for the 5h window, `weekly_all` for the 7d window, and
    `weekly_scoped` for per-model weekly windows.
  - `is_active` is ignored.
- The legacy top-level blocks `five_hour` and `seven_day`, each with
  `utilization` (0-100, used) and `resets_at`, and sometimes `used_dollars` and
  `limit_dollars`.
  - quota reads only these. clauth falls back to them when `limits[]` lacks an
    entry.
  - clauth notes the top-level `seven_day_*` model fields are now null.
  - quota also reads `seven_day_sonnet`, `seven_day_sonnet_4` and
    `seven_day_model`.

Other blocks in the response:

- `extra_usage {is_enabled, monthly_limit, used_credits, utilization, currency,
  ...}`
- `spend {enabled, used, limit, percent}`

The token comes from `~/.claude/.credentials.json`, under `claudeAiOauth`:
`accessToken`, `refreshToken`, `expiresAt` (epoch ms), `scopes[]`,
`subscriptionType`, `rateLimitTier` and more. With `CLAUDE_CONFIG_DIR` set the
file is `<dir>/.credentials.json`. On macOS Claude Code keeps it in the Keychain
instead, which doesn't matter for shepr.

The plan and account identity come from `GET
https://api.anthropic.com/api/oauth/profile`. clauth sends it with
`User-Agent: axios/1.15.2` and no beta header. The fields read are:

- `account.{uuid, email, has_claude_max, has_claude_pro}`
- `organization.{uuid, organization_type, rate_limit_tier,
  subscription_status}`
- `organization_type` is one of `claude_max`, `claude_pro`, `claude_team(s)`,
  `claude_enterprise` or `claude_free`.
- `rate_limit_tier` values look like `default_claude_max_5x`, which clauth
  shows as "Max 5x".

clauth fetches `/profile` at most once per hour per account.

Throttling, all from clauth:

- Anthropic answers every `/usage` 429 with `retry-after: 0`, and rejected
  polls count against the account's window. Treating 0 as "retry now" made
  things worse, and clauth reverted it.
- clauth's backoff ladder is interval + 10 s x 3^(streak-1), with the gap
  capped at max(interval, 5 min). A real non-zero hint is honoured up to
  15 min.
- It spaces requests to the same host at least 5 s apart, process-wide.
- The default cadence is 90 s, clamped to 10 s through 1 h, with a
  deterministic per-account jitter of up to interval/4. Its history also tried
  adaptive, 40 s and 60 s cadences.
- It does one forced poll 15 s after a 5h `resets_at`.
- A canceled subscription was seen 429ing `/usage` on every tick while
  `/profile` still answered (`subscription_status: canceled`).
- An idle 5h window reports `resets_at` about 5h out, which looks the same as a
  window that just opened.
- `claude setup-token` tokens carry only `user:inference
  user:sessions:claude_code`, without `user:profile`, and probably cannot read
  `/usage`.

Token refresh, if anyone does it:

- `POST https://platform.claude.com/v1/oauth/token` with a JSON body:
  `grant_type=refresh_token`, `refresh_token`,
  `client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e` (Claude Code's own) and
  `scope`.
- Refresh tokens are single-use and rotate.
- clauth and quota's desktop app refresh. quota's CLI deliberately does not.
- clauth's history is the cautionary tale. Its rotation policy flipped four
  times, and it lost accounts to "refresh-race account deaths" when several
  sessions and clauth rotated the same chain.
- Claude Code re-reads credentials only when the file's mtime changes.

Pros:

- It is authoritative and live.
- It works for idle accounts as long as the access token hasn't expired.

Cons:

- It needs the access token.
- An expired token means a stale reading unless something refreshes it, and
  refreshing means joining the token race.
- The endpoint is undocumented and rate-limited per account.
- Avoiding heavy throttling probably means impersonating the user agent.

## Method B: Claude, the statusLine payload

Used by herdr-agent-usage and, optionally, herdr-agent-usage2.

Claude Code passes JSON on stdin to the configured `statusLine.command`. It
contains:

- `rate_limits.five_hour.{used_percentage, resets_at}` and
  `rate_limits.seven_day.{used_percentage, resets_at}`
  - `resets_at` is in Unix seconds, or an RFC 3339 string in Claude Code
    2.1.233.
  - This is present only for Claude.ai subscribers, and only after the
    session's first API response. A missing block means "no sample", not zero.
- Also `session_id`, `transcript_path`, `model.display_name`,
  `context_window.{used_percentage, current_usage}`,
  `prompt_cache.{warm, expires_at, ttl}` (Claude Code 2.1.251 or newer) and
  `cost.total_api_duration_ms`.

How herdr-agent-usage collects it:

- It rewrites `statusLine.command` in `$CLAUDE_SETTINGS_FILE`, or else
  `$CLAUDE_CONFIG_DIR/settings.json`, or else `~/.claude/settings.json`.
- The user's original command is backed up and chained, with a deadline.
- It forces `refreshInterval` to 60 s.
- It writes one observation file that every session shares. The
  read-modify-write races, so two sessions can lose an update.
- It only wraps one profile's settings.

herdr-agent-usage2 asks the user to chain the statusLine by hand, and caches
per profile in `<config_dir>/herdr-usagebar/claude-limits-latest.json`.

herdr-agent-usage tells real API responses apart from redraws with an
`api_generation` fingerprint: a sha256 of the cost, token and usage counters.

Pros:

- No tokens and no network.
- It is the data Claude Code itself was given.

Cons:

- It only exists while a session is running and has made a call, so an idle
  account shows nothing.
- It occupies the single statusLine slot.
- It needs one wrapper per `CLAUDE_CONFIG_DIR`.

In shepr, Claude Code's hooks don't receive `rate_limits`; only the statusLine
command does. So using this through shepr's integration means installing a
statusLine wrapper, not just a hook.

## Method C: Claude, `cachedUsageUtilization` in `.claude.json`

Used by herdr-agent-usage2. clauth knows the field but only carries it along as
per-profile data.

Claude Code caches its last usage reading in `.claude.json`:

- The fields are `cachedUsageUtilization.fetchedAtMs` and
  `cachedUsageUtilization.utilization.five_hour` and `.seven_day`, each with
  `utilization` (used percent) and `resets_at` (ISO 8601).
- clauth's test fixture shows a per-account shape:
  `{accountUuid, utilization:{five_hour:{utilization}, ...}}`.
- herdr-agent-usage2 hard-codes the window lengths (300 and 10080 min) and
  labels the reading stale after 120 min.
- The key has already been renamed once (herdr-agent-usage2 commits 18809ee
  and bce3fb9).

Where `.claude.json` lives:

- The default `~/.claude` config dir pairs with `~/.claude.json`.
- Any other `CLAUDE_CONFIG_DIR` uses `<dir>/.claude.json`.

The same file has the account identity under `oauthAccount`:

- `emailAddress`, `accountUuid` and `organizationUuid`
- the plan in `organizationType` or `seatTier`
- `userRateLimitTier`, `organizationRateLimitTier`, and a legacy plural form of
  the latter

Pros:

- No tokens and no network, and no setup at all.

Cons:

- It is only as fresh as Claude Code's last fetch.
- It is an undocumented internal whose key names have already changed.
- It is unverified whether current Claude Code still writes it. Check a live
  file.

## Method D: Codex, the `wham/usage` endpoint

Used by clauth and by quota's CLI and desktop app.

The request is `GET https://chatgpt.com/backend-api/wham/usage` with these
headers:

- `Authorization: Bearer <tokens.access_token>`
- `ChatGPT-Account-Id: <account id>`
- clauth also sends `X-OpenAI-Fedramp: true` when
  `tokens.chatgpt_account_is_fedramp` is set.
- clauth sends `User-Agent: codex_cli_rs/<ver> (<os> <osver>; <arch>)
  <terminal>`, ported from Codex's own builder. It suppresses the `Accept`
  header.
- quota sends `Accept: application/json` and `User-Agent: quota`.

The response:

- `plan_type`
- `rate_limit.limit_reached`
- `rate_limit.primary_window` and `rate_limit.secondary_window`, each with
  `used_percent`, `limit_window_seconds`, `reset_after_seconds` and `reset_at`
  (epoch s)
- `rate_limit_reached_type{type}`
- `rate_limit_reset_credits{available_count}`

Choosing which window is which:

- clauth treats a window longer than 24 h as weekly and anything shorter as
  5h, using position only to break ties. An absolute `reset_at` beats
  `reset_after_seconds`. `limit_reached` forces the fuller window to 100%.
- quota's CLI labels each window from `limit_window_seconds`. Its desktop app
  hard-codes primary as 5h and secondary as weekly.

The token comes from `$CODEX_HOME/auth.json`, defaulting to `~/.codex`. Its
shape is `{auth_mode, OPENAI_API_KEY, tokens{id_token, access_token,
refresh_token, account_id, chatgpt_account_is_fedramp}, last_refresh}`.

- The `id_token` JWT payload, decoded without verification, carries
  `https://api.openai.com/auth` with `chatgpt_account_id`, `account_id` and
  `chatgpt_plan_type`, and `https://api.openai.com/profile` with the email.
- An API-key login has no quota.

Codex `auth.json` facts, from clauth's `docs/codex-plan.md`:

- `auth.json` is written in place, with no rename, so a reader can catch a torn
  file. Treat a short or unparseable read as "retry", not "logged out".
- `cli_auth_credentials_store = keyring|auto` means `auth.json` is absent or
  stale by design.
- Codex re-reads `auth.json` before spending a refresh token.

Token refresh:

- `POST https://auth.openai.com/oauth/token` with
  `client_id=app_EMoamEEZ73f0CkXaXp7hrann`.
- The refresh token is single-use. Re-sending one gives
  `refresh_token_reused`, and only a browser re-login recovers. clauth keeps a
  memo on disk so it never replays a token.
- quota's Codex import copies the CLI's refresh token, so whichever side
  refreshes first invalidates the other.

Pros:

- It is authoritative and live.

Cons:

- It needs the access token, and the same refresh hazards apply as for Claude,
  only worse.
- It is undocumented.

## Method E: Codex, JSON-RPC to `codex app-server`

Used by herdr-agent-usage.

Steps:

1. Spawn `codex app-server --stdio`.
   - The binary is `$CODEX_BIN_PATH`, else found on `PATH`, else
     `~/.local/bin`, `/opt/homebrew/bin` or `/usr/local/bin`.
   - It runs in its own process group, behind a 15 s watchdog that kills the
     group.
2. Speak newline-delimited JSON-RPC 2.0:
   - `initialize` with `clientInfo {name, version}` and empty `capabilities`
   - the `initialized` notification
   - `account/read`
   - `account/rateLimits/read`
   - herdr-agent-usage also calls `thread/list`, which isn't needed for usage.
3. Read the response:
   - `result.rateLimits.{primary, secondary}`, plus every entry of
     `rateLimitsByLimitId`
   - snake_case aliases are accepted too
   - each window has `usedPercent`, `windowDurationMins` and `resetsAt` (Unix
     s or a string)
4. Classify each window by its duration:
   - 300 min +-60 is the 5h window, and 10080 min +-180 is the weekly one.
   - Anything else is silently dropped, and the first window of each kind wins.

The account id comes from `auth.json`, reading only `tokens.account_id` or
`tokens.chatgpt_account_id`, or from the `account/read` result. API-key logins
are rejected.

Codex refreshes its own tokens; the caller never touches one.

Pros:

- It is live, uses no tokens, and the refresh logic is Codex's own.

Cons:

- It starts a full Codex process. herdr-agent-usage does this about once a
  minute; a long-lived server could keep one process per `CODEX_HOME`.
- The wire format isn't covered by any schema.
- It needs `codex` installed on the host.

## Method F: Codex, `rate_limits` in rollout files

Used by herdr-agent-usage2. memex reads the same events but ignores
`rate_limits`.

The files are `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl`. Some are also
`.jsonl.zst`, and there is an `archived_sessions/` directory as well.

How herdr-agent-usage2 reads them:

1. List the rollouts newest first by mtime and scan up to 25 of them.
2. Read the last 512 KiB of each.
3. Scan the lines backwards for the first line with `type=="event_msg"`,
   `payload.type=="token_count"` and `payload.rate_limits`, or
   `payload.info.rate_limits`.
4. Read `primary` and `secondary`, each with `used_percent`, `window_minutes`
   and `resets_at` (epoch s), plus `plan_type`.
5. Label each window from `window_minutes`: 360 or less is 5h, 9000 to below
   28800 is 7d, and above that is 30d.
6. The observed time is the file's mtime, and the reading is labelled stale
   after 30 min.

Because the limits are account-wide, the cwd is ignored.

herdr-agent-usage2 walks and `stat`s the whole `sessions/` tree on every
collect, every 15 s. That grows with history, and only recent date directories
are needed.

Pros:

- No tokens, no network and no setup.

Cons:

- It is only as fresh as that account's last Codex API call.
- It depends on an undocumented JSONL shape.

## Method G: borrowing another agent's observation

herdr-agent-usage2 reads OMP's `~/.omp/agent/agent.db` `usage_history` table.
The limit ids it uses are `anthropic:5h` and `anthropic:7d`, and ids ending in
`:primary` and `:secondary` for Codex.

It only borrows when the email or account id matches strictly. Borrowed rows
say where they came from and how old they are. This is niche and probably not
relevant to shepr.

## Not quota: token counting from transcripts

memex, and clauth's `tokens.rs`, read `~/.claude/projects/**/*.jsonl`
(`message.usage` on `type=assistant` lines) and Codex `token_count`
`info.total_token_usage`. That gives token counts and API-equivalent cost, not
plan limits. memex says outright that this is "not subscription quota".

memex dedupes Claude events by message id plus request id. It converts Codex's
cumulative totals into per-event deltas.

## Account discovery

None of the projects discovers accounts automatically.

- **clauth:** explicit profiles that you capture or log in, stored under
  `~/.clauth`.
  - It works out which account a session runs on in this order:
    1. match the `refreshToken` against stored profiles
    2. match the `accessToken` against the setup-token sidecar
    3. read the macOS Keychain
    4. read the `CLAUDE_CONFIG_DIR` or `CODEX_HOME` path shape
    5. fall back to the active profile
  - The identity anchor is the `/profile` `account.uuid`.
- **quota:**
  - The CLI and herdr plugin read one fixed file per provider and ignore
    `CLAUDE_CONFIG_DIR`, so every pane gets the same account's numbers.
  - The desktop app runs its own OAuth login per account, impersonating the
    official client ids.
- **herdr-agent-usage:**
  - Codex: one `CODEX_HOME`, taken from the server's environment.
  - Claude: the account is `sha256(accountUuid \0 organizationUuid)` from
    `.claude.json`, keyed by statusLine `session_id`.
- **herdr-agent-usage2:** a manual list of profiles in its config.
  - `[[claude.profiles]]` with `config_dir`, and `[[codex.profiles]]` with
    `codex_home`.
  - A bad config silently falls back to defaults.

Identity keys reported:

- Claude:
  - `oauthAccount.accountUuid` plus `organizationUuid` from `.claude.json`
  - or `account.uuid` and `organization.uuid` from `/profile`
  - the email from `oauthAccount.emailAddress`
- Codex:
  - `tokens.account_id`, or `chatgpt_account_id` in the `id_token`
  - the email from the `id_token` profile claim

Suggested by several agents for shepr: read `CLAUDE_CONFIG_DIR` and
`CODEX_HOME` from `/proc/<pid>/environ` of each detected agent process, add the
default directories, and dedupe by account identity. An open question is
whether the client should merge one account seen on several hosts.

## Freshness and presentation ideas

- **Reset time:**
  - Drop or mark a window once its `resets_at` has passed. clauth and
    herdr-agent-usage do this. herdr-agent-usage2 deliberately keeps the old
    number, which is a known weakness.
  - herdr-agent-usage forces a re-fetch when a window has expired.
- **Timestamps:**
  - Stamp `fetched_at` when the fetch happens, and treat a future-dated stamp
    as stale (clauth).
  - A missing timestamp must not read as "now"; herdr-agent-usage2 gets this
    wrong.
- **Missing percent:** treat it as unknown, never as 0% used. clauth and
  quota's desktop app get this wrong.
- **Failures:** keep the last good reading after a failed fetch, but only while
  the account still matches (herdr-agent-usage).
- **Wrong account:** prefer missing data over a wrong-account number
  (herdr-agent-usage and herdr-agent-usage2).
- **One fetcher per host:** the others render from cache. clauth uses a flock
  lease; one shepr server per host gives this for free.
- **What to send:** absolute epochs and data, never rendered strings. quota's
  plugin caches display strings, which loses the reset time.
- **Run-out projection** (herdr-agent-usage2): short windows use the recent
  pace, long windows the elapsed-average pace with a 12 h floor. The result is
  shown as "empty in ~Xh".
- **Display:**
  - A used/remaining toggle. herdr-agent-usage2 defaults to remaining; clauth
    shows used.
  - Low-quota alerts at thresholds: quota defaults to 20%; herdr-agent-usage2
    uses 50, 20, 10 and 5%, once per window and bucket.

## Credential hygiene seen

- Redact tokens in hand-written `Debug` output (quota). clauth's
  `TokenFailure` deliberately has no `Display`.
- Report serde errors as line and column only, and HTTP error bodies by length
  only (quota).
- Deserialize only the id fields from `auth.json` and `.claude.json`
  (herdr-agent-usage and herdr-agent-usage2).
- Write files 0600 and directories 0700, atomically (clauth). quota writes
  tokens at the default umask.
- Give every request a timeout. clauth and quota both lack body or overall
  timeouts.

## Open decisions for shepr

1. **Claude source:**
   - Method A needs the token and is throttled.
   - Method C is passive and possibly enough.
   - Method B needs statusLine wrapping and only works for live sessions.
   - Some combination, newest reading wins.
2. **Codex source:** Method D needs the token. Method E needs a Codex process
   and no token. Method F is passive. Or some combination.
3. **Refreshing tokens:** whether shepr ever does it. Every report recommends
   never: show an expired token's account as stale instead.
4. **User agent:** whether to send Claude Code's or Codex's when calling the
   endpoints.
5. **Account dedupe:** whether the client merges one account seen on several
   hosts.
