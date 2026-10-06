#!/bin/sh
# installed by shepr
# managed by shepr; every release shepr server launch on this host rewrites this file.
# add custom hooks beside this file instead of editing it.

set -eu

# Normal gate and report paths finish here, so the agent gets a clean exit.
finish() {
@FINISH_TRAP_RESET@
  cat >/dev/null 2>/dev/null || true
@FINISH_BODY@
  exit 0
}

@EXIT_TRAP@
@EARLY_SEQ@
action="${1:-}"
trap 'finish' HUP INT TERM

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
SHEPR_ACTION="$action" SHEPR_HOOK_SEQ="${hook_seq:-}" python3 -c '
@PYTHON@
' 2>/dev/null || true

finish
