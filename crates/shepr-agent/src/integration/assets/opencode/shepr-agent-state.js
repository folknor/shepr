// installed by shepr
// managed by shepr; reinstalling or updating the integration overwrites this file.
// add custom hooks/plugins beside this file instead of editing it.
// SHEPR_INTEGRATION_ID=opencode
// SHEPR_INTEGRATION_VERSION=3

import net from "node:net";

const SOURCE = "shepr:opencode";
const AGENT = "opencode";
// Seqs are microseconds since the epoch plus one per report, while the shell
// and Python hooks send nanoseconds. The units never meet: shepr orders seqs
// per source string, and the only other reporter under this source, the TUI
// plugin, uses the same unit (and never runs alongside this server plugin; see
// `ownsLocalLifecycle`). Nanoseconds are not an option here: they exceed 2^53,
// where a JS number stops being exact, so `+= 1` would round away. The
// wall-clock seed puts a restarted process above its predecessor's last seq;
// after a backwards clock step, shepr accepts any seq from a source that has
// been silent for a few seconds.
let reportSeq = Date.now() * 1000;
let requestChain = Promise.resolve();
let reportedRootSessionID;
let reportedLocalSessionID;

// Track child sessions so their events cannot replace the pane's root session.
// User prompts carry the root id to preserve its identity and cross-talk guard.
const childSessions = new Map();
const CHILD_EVENT_STATES = new Map([
  ["permission.asked", "blocked"],
  ["question.asked", "blocked"],
  ["permission.replied", "working"],
  ["question.replied", "working"],
  ["question.rejected", "working"],
]);

function nextReportSeq() {
  reportSeq += 1;
  return reportSeq;
}

function sessionIDFromProperties(properties) {
  if (typeof properties?.sessionID === "string" && properties.sessionID) {
    return properties.sessionID;
  }
  return typeof properties?.info?.id === "string" && properties.info.id
    ? properties.info.id
    : undefined;
}

const SESSION_STATE_BY_STATUS = new Map([
  ["idle", "idle"],
  ["active", "working"],
  ["busy", "working"],
  ["pending", "working"],
  ["retry", "working"],
  ["running", "working"],
  ["streaming", "working"],
  ["working", "working"],
]);

function stateFromSessionStatus(status) {
  const kind = typeof status === "string" ? status : status?.type;
  return typeof kind === "string"
    ? SESSION_STATE_BY_STATUS.get(kind.toLowerCase())
    : undefined;
}

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
    // that never finishes connecting still settles within 500 ms.
    timer = setTimeout(settle, 500);
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
  return request("pane.report_agent_session", params);
}

function reportState(state, sessionID) {
  if (!sessionID) {
    return Promise.resolve();
  }

  const params = { state };
  reportedRootSessionID = sessionID;
  params.agent_session_id = sessionID;
  return request("pane.report_agent", params);
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
  if (
    !ownsLocalLifecycle() ||
    process.env.SHEPR_BUILD_PROFILE !== "release" ||
    process.env.SHEPR_ENV !== "1" ||
    !process.env.SHEPR_SOCKET_PATH ||
    !process.env.SHEPR_PANE_ID
  ) {
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
        await reportSession(sessionID, "startup");
      }
      await reportState("working", sessionID);
    },
    event: async ({ event }) => {
      const type = event?.type;
      const properties = event?.properties ?? {};
      const sessionID = sessionIDFromProperties(properties);

      const info = properties.info;
      if (info?.id && info.parentID) {
        childSessions.set(info.id, info.parentID);
      }
      if (sessionID && childSessions.has(sessionID)) {
        const state = CHILD_EVENT_STATES.get(type);
        if (state) {
          let rootSessionID = sessionID;
          const seen = new Set();
          while (childSessions.has(rootSessionID) && !seen.has(rootSessionID)) {
            seen.add(rootSessionID);
            rootSessionID = childSessions.get(rootSessionID);
          }
          await reportState(state, rootSessionID);
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
          if (sessionID && sessionID !== reportedRootSessionID) {
            await reportSession(sessionID);
          }
          break;
        case "session.status": {
          const state = stateFromSessionStatus(properties.status);
          if (state) {
            await reportState(state, sessionID);
          } else {
            await reportSession(sessionID);
          }
          break;
        }
        case "tool.execute.before":
        case "tool.execute.after":
        case "permission.replied":
        case "question.replied":
        case "question.rejected":
        case "session.compacted":
          await reportState("working", sessionID);
          break;
        case "permission.asked":
        case "question.asked":
          await reportState("blocked", sessionID);
          break;
        case "session.error":
          // Escape aborts a request without leaving the session blocked.
          if (properties.error?.name !== "MessageAbortedError") {
            await reportState("blocked", sessionID);
          }
          break;
        case "session.idle":
          await reportState("idle", sessionID);
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
