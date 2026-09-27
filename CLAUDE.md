@AGENTS.md

## Bash rules
- Never use sed, find, awk, or complex bash commands. Write a script instead.
- Never chain commands with &&. Write a script instead.
- Never chain commands with ;. Write a script instead.
- Never pipe commands with |. Write a script instead.
- Never capture stdout into env vars (`UUID=$(...)`) - shell state doesn't persist between tool calls. Read the output directly and use the value inline.
- Never read or write from /tmp. All data lives in the project.
- Never use raw `cargo`. Use `brokkr check` or `brokkr test`.

## Communication rules
- Shouting is illegal. No all-caps words for emphasis.
- Never use the `AskUserQuestion` tool - the harness runs in don't-ask mode and it will be denied. When you need a decision from the user, just ask in chat with the options laid out in prose.

## Memory rules
Do not use your Memory functionality. Do not read, write, or update memories. Do not suggest saving things to memory. Durable context belongs in CLAUDE.md or the relevant docs.

## git commit rules
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

## Codex agents (via `review`)
Never tell a codex agent to read CLAUDE.md (it is Claude-specific and contradicts
their job), and never tell them to read AGENTS.md (codex loads it automatically).
Put any rule they need directly in the prompt.

## Subagents
Subagents must NOT run any shell commands. They write code only. Integration, building, and testing is done in the main conversation.
