#!/bin/sh
# installed by shepr
# managed by shepr; every release shepr server launch on this host rewrites this file.
# add custom hooks beside this file instead of editing it.
# SHEPR_INTEGRATION_ID=@ID@
# SHEPR_INTEGRATION_VERSION=@VERSION@

set -eu

# Every exit path of the hook ends here, so the agent always sees a clean exit.
finish() {
@FINISH_BODY@
  exit 0
}

@EARLY_SEQ@
action="${1:-}"
hook_input_file="$(mktemp "${TMPDIR:-/tmp}/shepr-@LABEL@-hook.XXXXXX")" || {
  cat >/dev/null 2>/dev/null || true
  finish
}
trap 'rm -f "$hook_input_file"' 0
trap 'finish' HUP INT TERM
cat >"$hook_input_file" 2>/dev/null || true

@ACTION_GATE@
# Shared agent configs contain release hooks only. Dev panes use detection.
[ "${@ENV_PROFILE@:-}" = "@PROFILE_RELEASE@" ] || finish
[ "${@ENV_MARKER@:-}" = "@ENV_MARKER_VALUE@" ] || finish
[ -n "${@ENV_SOCKET@:-}" ] || finish
[ -n "${@ENV_PANE@:-}" ] || finish
@SHELL_GATE@
command -v python3 >/dev/null 2>&1 || finish

# A python failure must not fail the hook: under `set -eu` it would exit
# non-zero with a traceback on stderr, which the agent may show to the user.
SHEPR_ACTION="$action" SHEPR_HOOK_INPUT_FILE="$hook_input_file" SHEPR_HOOK_SEQ="${hook_seq:-}" python3 - 2>/dev/null <<'PY' || true
@PYTHON@
PY

finish
