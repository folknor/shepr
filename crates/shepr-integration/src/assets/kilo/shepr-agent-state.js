// installed by shepr
// managed by shepr; every release shepr server launch on this host rewrites this file.
// add custom hooks/plugins beside this file instead of editing it.
// SHEPR_INTEGRATION_ID=kilo
// SHEPR_INTEGRATION_VERSION=6

import net from "node:net";

const SOURCE = "shepr:kilo";
const AGENT = "kilo";
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
// clock step, shepr re-anchors only if wall time is earlier than it was at the
// last accepted report or has fallen seconds behind monotonic time. Silence alone
// never permits re-anchoring.
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
