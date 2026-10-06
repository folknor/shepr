import net from "node:net";

const SOURCE = "@SOURCE@";
const METHOD_SESSION = "@METHOD_SESSION@";
const METHOD_STATE = "@METHOD_STATE@";
const START = @START_JS@;
const SOCKET_WAIT_MS = @SOCKET_WAIT_MS@;
const STATE = @STATES_JS@;
// The start source of the report that selects the pane's session.
const SELECTION_START_SOURCE = START.select;

// Only a release pane of a shepr server has anything to report to.
function reportingEnabled() {
  return (
    process.env.@ENV_PROFILE@ === "@PROFILE_RELEASE@" &&
    process.env.@ENV_MARKER@ === "@ENV_MARKER_VALUE@" &&
    !!process.env.@ENV_SOCKET@ &&
    !!process.env.@ENV_PANE@
  );
}

function paneEndpoint() {
  return {
    paneId: process.env.@ENV_PANE@,
    socketPath: process.env.@ENV_SOCKET@,
  };
}

@SEQ_UNITS_NOTE@
function seedSeq() {
  return Date.now() * 1000;
}

// Delivers one report: a state report, or with no `state` the selection of
// `sessionID` as the pane's session. Settles true once the server answers and
// false when it did not, so the caller can retry.
function requestOnce(sessionID, state, seq, isCurrent = () => true) {
  const { paneId, socketPath } = paneEndpoint();
  if (!paneId || !socketPath) {
    return Promise.resolve(true);
  }

  // A selection report has no seq; its id takes a clock reading in the seq's
  // unit instead, so the id keeps the `<source>:<seq>` shape of every hook.
  const request = {
    id: `${SOURCE}:${state === undefined ? seedSeq() : seq}`,
    method: state === undefined ? METHOD_SESSION : METHOD_STATE,
    params: {
      pane_id: paneId,
      source: SOURCE,
      agent_session_id: sessionID,
      ...(state === undefined ? { session_start_source: SELECTION_START_SOURCE } : { state, seq }),
    },
  };

  return new Promise((resolve) => {
    let settled = false;
    let timer;
    const settle = (delivered) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      client.destroy();
      resolve(delivered);
    };
    const client = net.createConnection(socketPath, () => {
      if (!isCurrent()) {
        settle(false);
        return;
      }
      client.write(`${JSON.stringify(request)}\n`);
    });

    // A plain timer, not socket.setTimeout, so a connection that never finishes
    // connecting still settles and cannot block later reports behind the queue.
    timer = setTimeout(() => settle(false), SOCKET_WAIT_MS);
    timer.unref?.();
    client.on("data", () => settle(true));
    client.on("error", () => settle(false));
    client.on("end", () => settle(false));
    client.on("close", () => settle(false));
  });
}
