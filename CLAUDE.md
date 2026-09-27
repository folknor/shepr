@AGENTS.md

## More rules

### Memory rules

Do not use your Memory functionality. Durable context belongs in CLAUDE.md or the relevant docs.

### Communication rules
- Shouting is illegal. No all-caps words for emphasis.
- Never use the `AskUserQuestion` tool - the harness runs in don't-ask mode and it will be denied. When you need a decision from the user, just ask in chat with the options laid out in prose.

### Bash rules

- Never use `sed`, `find`, `awk`, `head`, `tail`, or complex bash commands.
- Never `find /`.
- One Bash() invocation === one command.
- Never chain commands with `&&`.
- Never chain commands with `;`.
- Never chain/pipe commands with `|`. Exception: piping into `review` is allowed.
- Never capture stdout into env vars (`UUID=$(...)`).
- Keep `git commit -m` messages free of zsh metacharacters - braces `{}`, brackets `[]`, parens `()`, angle brackets `<>`, `#`. They trip the permission matcher and block the commit. Spell lists out (`syntax, vm, data and runner`, not `{syntax,vm,data,runner}`), write `5.1 per bar` not `5.1/bar`, name attributes in prose not `#[attr]`.

### git commit rules
- Never run `git checkout --`, `git restore`, or `git stash` against working-tree
  changes. Other agents' uncommitted work may be sitting in those files, and it is
  unrecoverable once discarded - an agent has already destroyed a whole round of
  work this way. Discarding a dirty file is an operation only the user may ask for
  explicitly.
- Always run `brokkr fmt` before a commit.
- Never commit markdown changes alone - bundle them with the related code commit;
  tag along dirty markdown when committing other changes.
- Write substantive engineering-focused commit messages.
- Has `Cargo.lock` changed? Commit it.
- Never `git push` unless explicitly asked. Stop after the commit.

### Subagents

- Always get permission from the user before launching subagents.
- Do NOT use git worktree isolation for parallel agents. Worktrees create merge conflicts that silently drop agent work. Instead, launch agents in the same tree with strict file ownership - zero overlap.

Agent coordination rules:
- Each agent gets exclusive ownership of specific files. No two agents touch the same file.
- Agents must read their target file FIRST. Do not replace existing code with placeholders or stub it out.
- Agents must NOT run `brokkr` or `cargo`. The orchestrator validates between agents.

Audit protocol:
- Do not trust agent claims of completion. Verify existence + wiring + behavior.
- Use the 3-pass audit structure: domain-specific verification, then cross-cutting reconciliation (is the new code actually wired into its callers?), then editorial normalization.
- Any discrepancies doc should contain only current gaps, not historical records. Remove resolved items entirely.

Subagent prompt rules:
- Scope the investigation, not the report. Caps like "under 1500 chars" or "max 15 findings" throw away signal you asked them to surface.
- Invite lateral findings up front. If they notice a bug, optimization, smell, or anything surprising while doing the scoped work, they should flag it, even when it's outside the immediate task.
- Name the question, not the method. Don't prescribe tools ("use `git diff`", "use `Read`"), don't prescribe steps ("read in full, not just hunks"), don't enumerate files when the scope already implies them. Prescribing the method wastes tokens and signals distrust.
- Don't restate rules the agent already inherits. Subagents load the same CLAUDE.md / AGENTS.md as the main session, so the bash rules, no-cargo, no-worktrees, gremlins, etc. are already in scope. Re-listing them is noise.
- Do pass anything learned in *this* conversation that the agent can't see: the user's framing, prior decisions, what's already been ruled out, the specific claim being audited.

### Codex agents

Never tell a codex agent to read CLAUDE.md (it is Claude-specific and contradicts their job), and never tell them to read AGENTS.md (codex loads it automatically). Put any rule they need directly in the prompt.
