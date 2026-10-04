
// The TUI plugin reports under this source too and uses the same seq unit; it
// never runs alongside this server plugin (see `ownsLocalLifecycle`).
let reportedRootSessionID;
let reportedLocalSessionID;

// The local root's recognized start. Only the TUI integration reports
// `select`, which can replace an existing OpenCode root.
const LOCAL_START_SOURCE = "startup";

// A state report also records which root session this process last spoke for.
function reportRootState(state, sessionID) {
  if (sessionID) {
    reportedRootSessionID = sessionID;
  }
  return reportState(state, sessionID);
}

// A session report for the local root re-sends its recognized start, as Kilo
// does on every session update. Each report gets one attempt, and shepr parks
// state reports until a start anchors the session, so a start lost once (a
// busy socket, a server restarting) would leave the pane unanchored for the
// whole run. The plugin cannot see whether shepr accepted it; a repeat for the
// session shepr already holds selects that same session again and changes
// nothing. Any other session keeps its unrecognized report: server-global
// events may belong to an attached client.
function reportSessionOf(sessionID) {
  return reportSession(
    sessionID,
    sessionID === reportedLocalSessionID ? LOCAL_START_SOURCE : undefined,
  );
}

function ownsLocalLifecycle() {
  const args = process.argv.slice(2);
  const separator = args.indexOf("--");
  if (separator !== -1) args.splice(separator);
  if (args.some((arg) => arg === "--attach" || arg.startsWith("--attach="))) return false;
  while (args[0] === "--print-logs" || args[0] === "--log-level" || args[0]?.startsWith("--log-level=")) {
    args.splice(0, args[0] === "--log-level" ? 2 : 1);
  }
  // These local clients have no TUI plugin. Shared servers and the TUI worker
  // cannot identify their attached panes; their lifecycle belongs to each TUI.
  return args[0] === "run" ||
    (!["serve", "web", "attach"].includes(args[0]) && args.includes("--mini"));
}

export const SheprAgentStatePlugin = async () => {
  if (!ownsLocalLifecycle() || !reportingEnabled()) {
    return {};
  }

  return {
    "chat.message": async ({ sessionID }) => {
      if (sessionID && childSessions.has(sessionID)) {
        return;
      }
      // Event-bus session events are server-global. The local chat hook is the
      // first point that identifies this run's root session for the pane.
      if (sessionID && !reportedLocalSessionID) {
        reportedLocalSessionID = sessionID;
        // This recognized start anchors the first local identity; later
        // session events for it re-send it (`reportSessionOf`).
        await reportSession(sessionID, LOCAL_START_SOURCE);
      }
      await reportRootState(STATE.working, sessionID);
    },
    event: async ({ event }) => {
      const type = event?.type;
      const properties = event?.properties ?? {};
      const sessionID = sessionIDFromProperties(properties);

      trackChildSession(properties.info);
      if (sessionID && childSessions.has(sessionID)) {
        const state = CHILD_EVENT_STATES.get(type);
        if (state) {
          await reportRootState(state, rootSessionOf(sessionID));
        }
        return;
      }

      switch (type) {
        case "session.created":
          // Creation is server-global, so an attached client may own it. The
          // TUI plugin separately reports the root selected in this pane.
          reportedRootSessionID = sessionID;
          break;
        case "session.updated":
          if (
            sessionID &&
            (sessionID === reportedLocalSessionID || sessionID !== reportedRootSessionID)
          ) {
            await reportSessionOf(sessionID);
          }
          break;
        case "session.status": {
          const state = stateFromSessionStatus(properties.status);
          if (state) {
            await reportRootState(state, sessionID);
          } else {
            await reportSessionOf(sessionID);
          }
          break;
        }
        case "tool.execute.before":
        case "tool.execute.after":
        case "permission.replied":
        case "question.replied":
        case "question.rejected":
        case "session.compacted":
          await reportRootState(STATE.working, sessionID);
          break;
        case "permission.asked":
        case "question.asked":
          await reportRootState(STATE.blocked, sessionID);
          break;
        case "session.error":
          // Escape aborts a request without leaving the session blocked.
          if (properties.error?.name !== "MessageAbortedError") {
            await reportRootState(STATE.blocked, sessionID);
          }
          break;
        case "session.idle":
          await reportRootState(STATE.idle, sessionID);
          break;
        case "session.deleted":
          break;
        default:
          break;
      }
    },
  };
};

// V1 local run/Mini retain their server hooks. V1/V2 full TUIs own both
// selection and lifecycle, including when attached to a shared remote server.
export default {
  id: "shepr.opencode",
  server: SheprAgentStatePlugin,
  setup() {},
};
