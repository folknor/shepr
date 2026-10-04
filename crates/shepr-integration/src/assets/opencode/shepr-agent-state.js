// installed by shepr
// managed by shepr; every release shepr server launch on this host rewrites this file.
// add custom hooks/plugins beside this file instead of editing it.
// SHEPR_INTEGRATION_ID=opencode
// SHEPR_INTEGRATION_VERSION=4

import net from "node:net";

const SOURCE = "shepr:opencode";
const AGENT = "opencode";
const METHOD_SESSION = "pane.report_agent_session";
const METHOD_STATE = "pane.report_agent";
const SOCKET_WAIT_MS = 500;
const STATE = { working: "working", blocked: "blocked", idle: "idle" };
// Seqs are microseconds since the epoch plus one per report, while the shell
// and Python hooks send nanoseconds. The units never meet: shepr orders seqs
// per source string, and every JavaScript reporter under one source uses this
// unit. Nanoseconds are not an option here: they exceed 2^53, where a JS
// number stops being exact, so `+= 1` would round away. The wall-clock seed
// puts a restarted process above its predecessor's last seq; after a backwards
// clock step, shepr accepts any seq from a source that has been silent for a
// few seconds.
let reportSeq = Date.now() * 1000;
let requestChain = Promise.resolve();

// Only a release pane of a shepr server has anything to report to.
function reportingEnabled() {
  return (
    process.env.SHEPR_BUILD_PROFILE === "release" &&
    process.env.SHEPR_ENV === "1" &&
    !!process.env.SHEPR_SOCKET_PATH &&
    !!process.env.SHEPR_PANE_ID
  );
}

function nextReportSeq() {
  reportSeq += 1;
  return reportSeq;
}

// Reports go out one at a time so they reach shepr in sequence order.
function request(method, params) {
  const pending = requestChain.then(() => requestOnce(method, params));
  requestChain = pending.catch(() => {});
  return pending;
}

function requestOnce(method, params) {
  const paneId = process.env.SHEPR_PANE_ID;
  const socketPath = process.env.SHEPR_SOCKET_PATH;

  if (!paneId || !socketPath) {
    return Promise.resolve();
  }

  const seq = nextReportSeq();
  const request = {
    id: `${SOURCE}:${seq}`,
    method,
    params: {
      pane_id: paneId,
      source: SOURCE,
      agent: AGENT,
      seq,
      ...params,
    },
  };

  return new Promise((resolve) => {
    const client = net.createConnection(socketPath, () => {
      client.write(`${JSON.stringify(request)}\n`);
    });

    let timer;
    const settle = () => {
      clearTimeout(timer);
      client.destroy();
      resolve();
    };

    // A plain timer, not socket.setTimeout (an idle timeout), so a connection
    // that never finishes connecting still settles within the wait.
    timer = setTimeout(settle, SOCKET_WAIT_MS);
    timer.unref?.();
    client.on("data", settle);
    client.on("error", settle);
    client.on("end", settle);
    client.on("close", settle);
  });
}

function reportSession(sessionID, sessionStartSource) {
  if (!sessionID) {
    return Promise.resolve();
  }
  const params = { agent_session_id: sessionID };
  if (sessionStartSource) {
    params.session_start_source = sessionStartSource;
  }
  return request(METHOD_SESSION, params);
}

function reportState(state, sessionID) {
  if (!sessionID) {
    return Promise.resolve();
  }
  return request(METHOD_STATE, { state, agent_session_id: sessionID });
}

// Subagent (child) sessions must not speak for the pane: their created or
// updated reports would replace the resumable root session, and their idle
// would mark the pane idle while the root is still working. Only a child's
// prompts for the user (blocked) and the replies to them (working) are
// forwarded, attributed to the root session.
const childSessions = new Map();
const CHILD_EVENT_STATES = new Map([
  ["permission.asked", STATE.blocked],
  ["question.asked", STATE.blocked],
  ["permission.replied", STATE.working],
  ["question.replied", STATE.working],
  ["question.rejected", STATE.working],
]);

function sessionIDFromProperties(properties) {
  if (typeof properties?.sessionID === "string" && properties.sessionID) {
    return properties.sessionID;
  }
  return typeof properties?.info?.id === "string" && properties.info.id
    ? properties.info.id
    : undefined;
}

// A session payload names its parent when it is a subagent's.
function trackChildSession(info) {
  if (typeof info?.id === "string" && info.id && typeof info.parentID === "string" && info.parentID) {
    childSessions.set(info.id, info.parentID);
  }
}

function rootSessionOf(sessionID) {
  let rootSessionID = sessionID;
  const seen = new Set();
  while (childSessions.has(rootSessionID) && !seen.has(rootSessionID)) {
    seen.add(rootSessionID);
    rootSessionID = childSessions.get(rootSessionID);
  }
  return rootSessionID;
}

const SESSION_STATE_BY_STATUS = new Map([
  ["idle", STATE.idle],
  ["active", STATE.working],
  ["busy", STATE.working],
  ["pending", STATE.working],
  ["retry", STATE.working],
  ["running", STATE.working],
  ["streaming", STATE.working],
  ["working", STATE.working],
]);

// Status arrives either as a bare string or as an object such as
// `{ type: "busy" }` / `{ type: "retry", ... }`.
function stateFromSessionStatus(status) {
  const kind = typeof status === "string" ? status : status?.type;
  return typeof kind === "string"
    ? SESSION_STATE_BY_STATUS.get(kind.toLowerCase())
    : undefined;
}

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
