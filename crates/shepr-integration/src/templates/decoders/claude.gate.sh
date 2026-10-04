# A Claude background session (`/fork`, `/bg`, agent view) is its own process
# under Claude's supervisor, never the pane's process, yet its environment is
# built from the dispatching shell's and can carry the pane variables above.
# Claude sets CLAUDE_JOB_DIR on every background session (kept even where it
# strips its other CLAUDE_ variables) and CLAUDE_CODE_SESSION_KIND to `bg`;
# the supervisor's own kinds are `daemon` and `daemon-worker`.
[ -z "${CLAUDE_JOB_DIR:-}" ] || finish
case "${CLAUDE_CODE_SESSION_KIND:-}" in
  bg | daemon | daemon-worker) finish ;;
esac
