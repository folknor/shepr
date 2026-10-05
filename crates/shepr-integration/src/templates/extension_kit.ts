// @ts-nocheck

import net from "node:net";
import path from "node:path";

const @ENV_MARKER@ = process.env.@ENV_MARKER@;
const socketPath = process.env.@ENV_SOCKET@;
const paneId = process.env.@ENV_PANE@;
const source = "@SOURCE@";
const AGENT = "@LABEL@";
const METHOD_SESSION = "@METHOD_SESSION@";
const METHOD_STATE = "@METHOD_STATE@";
const START = @START_JS@;
const SOCKET_WAIT_MS = @SOCKET_WAIT_MS@;

// Only a release pane of a shepr server has anything to report to, and the
// agent's own decoder can stand the extension down further.
function enabled() {
  return (
    process.env.@ENV_PROFILE@ === "@PROFILE_RELEASE@" &&
    @ENV_MARKER@ === "@ENV_MARKER_VALUE@" &&
    !!socketPath &&
    !!paneId &&
    agentEnabled()
  );
}

let requestQueue = Promise.resolve();

function sendRequestAttempt(request: unknown, timeoutMs: number): Promise<boolean> {
  if (!enabled()) {
    return Promise.resolve(true);
  }

  return new Promise((resolve) => {
    let done = false;
    let timeout: ReturnType<typeof setTimeout> | undefined;
    const finish = (delivered: boolean) => {
      if (done) return;
      done = true;
      if (timeout) {
        clearTimeout(timeout);
      }
      socket.destroy();
      resolve(delivered);
    };

    const socket = net.createConnection(socketPath!);
    socket.on("error", () => finish(false));
    socket.on("connect", () => socket.write(`${JSON.stringify(request)}\n`));
    socket.on("data", () => finish(true));
    socket.on("end", () => finish(false));
    timeout = setTimeout(() => finish(false), timeoutMs);
    timeout.unref?.();
  });
}

// This retry is for socket delivery. It is not a state debounce: the agent's
// own events supply the state boundaries.
async function sendRequestNow(request: unknown): Promise<void> {
  if (await sendRequestAttempt(request, SOCKET_WAIT_MS)) {
    return;
  }
  await sendRequestAttempt(request, SOCKET_WAIT_MS);
}

function sendRequest(request: unknown): Promise<void> {
  // Keep both attempts in one slot so a retry cannot arrive after a newer seq.
  const pending = requestQueue.then(
    () => sendRequestNow(request),
    () => sendRequestNow(request),
  );
  requestQueue = pending.catch(() => {});
  return pending;
}

type AgentState = @STATE_UNION@;

type QueuedState = {
  state: AgentState;
  seq: number;
};

const STATE = @STATES_JS@;

@SEQ_UNITS_NOTE@
let reportSeq = Date.now() * 1000;
let currentAgentSessionId: string | undefined;
let currentAgentSessionPath: string | undefined;

function nextReportSeq(): number {
  reportSeq += 1;
  return reportSeq;
}

function isAbsoluteSessionPath(file: unknown): file is string {
  return typeof file === "string" && path.posix.isAbsolute(file);
}

function updateSessionRef(ctx: any): void {
  try {
    const file = ctx?.sessionManager?.getSessionFile?.();
    currentAgentSessionPath = isAbsoluteSessionPath(file) ? file : undefined;
  } catch {
    currentAgentSessionPath = undefined;
  }

  try {
    const id = ctx?.sessionManager?.getSessionId?.();
    currentAgentSessionId = typeof id === "string" && id.length > 0 ? id : undefined;
  } catch {
    currentAgentSessionId = undefined;
  }
}

function withSessionRef(params: Record<string, unknown>): Record<string, unknown> {
  if (currentAgentSessionPath) {
    return { ...params, agent_session_path: currentAgentSessionPath };
  }
  if (currentAgentSessionId) {
    return { ...params, agent_session_id: currentAgentSessionId };
  }
  return params;
}

function currentSessionRef(): Record<string, unknown> | undefined {
  if (currentAgentSessionPath) {
    return { agent_session_path: currentAgentSessionPath };
  }
  if (currentAgentSessionId) {
    return { agent_session_id: currentAgentSessionId };
  }
  return undefined;
}

function reportSession(sessionStartSource?: string): Promise<void> {
  const sessionRef = currentSessionRef();
  if (!sessionRef) {
    return Promise.resolve();
  }

  const seq = nextReportSeq();
  return sendRequest({
    id: `${source}:${seq}`,
    method: METHOD_SESSION,
    params: {
      pane_id: paneId,
      source,
      agent: AGENT,
      seq,
      ...(sessionStartSource ? { session_start_source: sessionStartSource } : {}),
      ...sessionRef,
    },
  });
}

function sendState(state: AgentState, seq = nextReportSeq()): Promise<void> {
  if (!currentSessionRef()) {
    return Promise.resolve();
  }

  return sendRequest({
    id: `${source}:${seq}`,
    method: METHOD_STATE,
    params: withSessionRef({
      pane_id: paneId,
      source,
      agent: AGENT,
      state,
      seq,
    }),
  });
}

let sendInFlight = false;
let queuedState: QueuedState | undefined;

function queueState(state: AgentState): void {
  queuedState = { state, seq: nextReportSeq() };
  if (!sendInFlight) {
    void drainStateQueue();
  }
}

async function drainStateQueue(): Promise<void> {
  if (sendInFlight) {
    return;
  }

  sendInFlight = true;
  try {
    while (queuedState) {
      const next = queuedState;
      queuedState = undefined;
      await sendState(next.state, next.seq);
    }
  } finally {
    sendInFlight = false;
    if (queuedState) {
      void drainStateQueue();
    }
  }
}
