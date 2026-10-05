import net from "node:net";

const SOURCE = "@SOURCE@";
const AGENT = "@LABEL@";
const METHOD_SESSION = "@METHOD_SESSION@";
const METHOD_STATE = "@METHOD_STATE@";
const START = @START_JS@;
const SOCKET_WAIT_MS = @SOCKET_WAIT_MS@;
const STATE = @STATES_JS@;
@SEQ_UNITS_NOTE@
let reportSeq = Date.now() * 1000;
let requestChain = Promise.resolve();

// Only a release pane of a shepr server has anything to report to.
function reportingEnabled() {
  return (
    process.env.@ENV_PROFILE@ === "@PROFILE_RELEASE@" &&
    process.env.@ENV_MARKER@ === "@ENV_MARKER_VALUE@" &&
    !!process.env.@ENV_SOCKET@ &&
    !!process.env.@ENV_PANE@
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
  const paneId = process.env.@ENV_PANE@;
  const socketPath = process.env.@ENV_SOCKET@;

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
