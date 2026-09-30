// installed by shepr
// managed by shepr; reinstalling or updating the integration overwrites this file.
// add custom hooks/plugins beside this file instead of editing it.
// SHEPR_INTEGRATION_ID=kilo
// SHEPR_INTEGRATION_VERSION=3

import net from "node:net";

const SOURCE = "shepr:kilo";
const AGENT = "kilo";
// Seqs are microseconds since the epoch plus one per report, while the shell
// and Python hooks send nanoseconds. The units never meet: shepr orders seqs
// per source string, and nothing else reports under this source. Nanoseconds
// are not an option here: they exceed 2^53, where a JS number stops being
// exact, so `+= 1` would round away. The wall-clock seed puts a restarted
// process above its predecessor's last seq; after a backwards clock step,
// shepr accepts any seq from a source that has been silent for a few seconds.
let reportSeq = Date.now() * 1000;
let requestChain = Promise.resolve();

// Kilo is a full-lifecycle authority for its pane, so subagent (child)
// sessions must not speak for the pane: their created/updated reports would
// replace the resumable root session, and their idle would mark the pane idle
// while the root is still working. Only a child's prompts for the user
// (blocked) and the replies to them (working) are forwarded, attributed to
// the root session.
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
  ["idle", "idle"],
  ["active", "working"],
  ["busy", "working"],
  ["pending", "working"],
  ["retry", "working"],
  ["running", "working"],
  ["streaming", "working"],
  ["working", "working"],
]);

// Status arrives either as a bare string or as an object such as
// `{ type: "busy" }` / `{ type: "retry", ... }`.
function stateFromSessionStatus(status) {
  const kind = typeof status === "string" ? status : status?.type;
  return typeof kind === "string"
    ? SESSION_STATE_BY_STATUS.get(kind.toLowerCase())
    : undefined;
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
    // that never finishes connecting still settles within 500 ms.
    timer = setTimeout(settle, 500);
    timer.unref?.();
    client.on("data", settle);
    client.on("error", settle);
    client.on("end", settle);
    client.on("close", settle);
  });
}

function reportSession(sessionID) {
  if (!sessionID) {
    return Promise.resolve();
  }
  // Kilo's session events carry no start source, so a resumed session cannot
  // be told apart from a new one here; "startup" is reported for both. Its
  // event payloads expose the ID either directly or through `info.id`, and
  // `updated` also fires for new sessions. shepr treats Kilo's "startup" and
  // "resume" alike: both can anchor a session, and neither lets Kilo replace it.
  return request("pane.report_agent_session", {
    agent_session_id: sessionID,
    session_start_source: "startup",
  });
}

function reportState(state, sessionID) {
  const params = { state };
  if (sessionID) {
    params.agent_session_id = sessionID;
  }
  return request("pane.report_agent", params);
}

export const SheprAgentStatePlugin = async () => {
  if (
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
      await reportState("working", sessionID);
    },
    event: async ({ event }) => {
      const type = event?.type;
      const properties = event?.properties ?? {};
      const sessionID = sessionIDFromProperties(properties);

      const info = properties.info;
      if (typeof info?.id === "string" && info.id && typeof info.parentID === "string" && info.parentID) {
        childSessions.set(info.id, info.parentID);
      }
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
          await reportSession(sessionID);
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
