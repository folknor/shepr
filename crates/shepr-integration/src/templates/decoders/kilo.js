
// Kilo is a full-lifecycle authority for its pane, so subagent sessions follow
// the child-session rules above.
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
  return !["acp", "attach", "console", "daemon", "serve", "web"].includes(args[0]);
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
