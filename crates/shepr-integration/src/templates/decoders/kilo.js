
// Kilo is a full-lifecycle authority for its pane, so subagent sessions follow
// the child-session rules above.
//
// Whether this process speaks for its pane is read from its own arguments. In
// the default TUI the plugin runs inside a Bun Web Worker whose `process.argv`
// is only `[execPath, workerScript]`, so the gate sees no arguments and owns
// the pane by design: that worker belongs to the TUI in this pane. Main-thread
// launches (`serve`, `acp`, and `run` or `--mini` when not attached to a
// daemon) see the real arguments, so the gate can tell them apart: `run` and
// `--mini` own the pane unless `--attach` names a daemon, while shared servers,
// attached clients and `remote` (a long-lived in-process instance relaying
// Kilo Cloud sessions) serve sessions that are not this pane's and never
// anchor to it.
function ownsLocalLifecycle() {
  const args = process.argv.slice(2);
  const separator = args.indexOf("--");
  if (separator !== -1) args.splice(separator);
  if (args.some((arg) => arg === "--attach" || arg.startsWith("--attach="))) {
    return false;
  }
  while (
    args[0] === "--print-logs" ||
    args[0] === "--log-level" ||
    args[0]?.startsWith("--log-level=")
  ) {
    args.splice(0, args[0] === "--log-level" ? 2 : 1);
  }
  return !["acp", "attach", "console", "daemon", "remote", "serve", "web"].includes(args[0]);
}

// Kilo's session events carry no start source, so "startup" is the only
// selection marker available for both new and resumed sessions. The mux
// allows it to replace the pane identity when this process owns the local
// lifecycle. Event payloads expose the ID directly or through `info.id`,
// and `updated` also fires for new sessions.
const SESSION_START_SOURCE = "startup";

export const SheprAgentStatePlugin = async () => {
  if (!ownsLocalLifecycle() || !reportingEnabled()) {
    return {};
  }

  return {
    "chat.message": async ({ sessionID }) => {
      if (sessionID && childSessions.has(sessionID)) {
        return;
      }
      await reportState(STATE.working, sessionID);
    },
    event: async ({ event }) => {
      const type = event?.type;
      const properties = event?.properties ?? {};
      const sessionID = sessionIDFromProperties(properties);

      trackChildSession(properties.info);
      if (sessionID && childSessions.has(sessionID)) {
        const state = CHILD_EVENT_STATES.get(type);
        if (state) {
          await reportState(state, rootSessionOf(sessionID));
        }
        return;
      }

      switch (type) {
        case "session.created":
        case "session.updated":
          await reportSession(sessionID, SESSION_START_SOURCE);
          break;
        case "session.status": {
          const state = stateFromSessionStatus(properties.status);
          if (state) {
            await reportState(state, sessionID);
          } else {
            await reportSession(sessionID, SESSION_START_SOURCE);
          }
          break;
        }
        case "tool.execute.before":
        case "tool.execute.after":
        case "permission.replied":
        case "question.replied":
        case "question.rejected":
        case "session.compacted":
          await reportState(STATE.working, sessionID);
          break;
        case "permission.asked":
        case "question.asked":
          await reportState(STATE.blocked, sessionID);
          break;
        case "session.error":
          // Escape aborts a request without leaving the session blocked.
          if (properties.error?.name !== "MessageAbortedError") {
            await reportState(STATE.blocked, sessionID);
          }
          break;
        case "session.idle":
          await reportState(STATE.idle, sessionID);
          break;
        case "session.deleted":
          break;
        default:
          break;
      }
    },
  };
};

// Kilo's server loader takes a default-exported descriptor first and calls only
// its `server`; a local-file plugin must name its `id`. The named export stays
// for loaders older than the descriptor (Kilo before its merge of OpenCode
// v1.3.4), which call every export in name order: the named export registers
// the hooks there, and the descriptor, which is not a function, is reported as
// a load error after it.
export default {
  id: "shepr.kilo",
  server: SheprAgentStatePlugin,
};
