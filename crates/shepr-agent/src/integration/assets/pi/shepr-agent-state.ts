// installed by shepr
// managed by shepr; reinstalling or updating the integration overwrites this file.
// add custom hooks/plugins beside this file instead of editing it.
// SHEPR_INTEGRATION_ID=pi
// SHEPR_INTEGRATION_VERSION=2
// @ts-nocheck

import net from "node:net";
import path from "node:path";

const SHEPR_ENV = process.env.SHEPR_ENV;
const socketPath = process.env.SHEPR_SOCKET_PATH;
const paneId = process.env.SHEPR_PANE_ID;
const source = "shepr:pi";
let requestQueue = Promise.resolve();

function enabled() {
  return SHEPR_ENV === "1" && !!socketPath && !!paneId;
}

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

// This retry is for socket delivery. Pi's agent_settled event supplies the
// state boundary, so it does not need OMP's state debounce or retry grace.
async function sendRequestNow(request: unknown): Promise<void> {
  if (await sendRequestAttempt(request, 500)) {
    return;
  }
  await sendRequestAttempt(request, 500);
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

type AgentState = "working" | "blocked" | "idle";

type QueuedState = {
  state: AgentState;
  seq: number;
};

// Seqs are microseconds since the epoch plus one per report, while the shell
// and Python hooks send nanoseconds. The units never meet: shepr orders seqs
// per source string, and nothing else reports under this source. Nanoseconds
// are not an option here: they exceed 2^53, where a JS number stops being
// exact, so `+= 1` would round away. The wall-clock seed puts a restarted
// process above its predecessor's last seq; after a backwards clock step,
// shepr accepts any seq from a source that has been silent for a few seconds.
let reportSeq = Date.now() * 1000;
let currentAgentSessionId: string | undefined;
let currentAgentSessionPath: string | undefined;

function nextReportSeq(): number {
  reportSeq += 1;
  return reportSeq;
}

function updateSessionRef(ctx: any): void {
  try {
    const file = ctx?.sessionManager?.getSessionFile?.();
    currentAgentSessionPath =
      typeof file === "string" && path.posix.isAbsolute(file) ? file : undefined;
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
    method: "pane.report_agent_session",
    params: {
      pane_id: paneId,
      source,
      agent: "pi",
      seq,
      session_start_source: sessionStartSource,
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
    method: "pane.report_agent",
    params: withSessionRef({
      pane_id: paneId,
      source,
      agent: "pi",
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

export default function (pi) {
  if (!enabled()) {
    return;
  }

  let agentActive = false;
  let blockedCount = 0;
  let lastState: AgentState | undefined;
  let rootSession = false;

  function desiredState() {
    if (blockedCount > 0) {
      return "blocked" as const;
    }
    if (agentActive) {
      return "working" as const;
    }
    return "idle" as const;
  }

  function publishState(force = false) {
    const next = desiredState();
    // Reports carry only state, so changed local prompt labels add no new information.
    if (!force && next === lastState) {
      return;
    }
    lastState = next;
    queueState(next);
  }

  pi.events.on("shepr:blocked", (data) => {
    if (!rootSession) {
      return;
    }
    if (!data?.active) {
      blockedCount = Math.max(0, blockedCount - 1);
      publishState();
      return;
    }

    blockedCount += 1;
    publishState();
  });

  pi.on("session_start", async (event, ctx) => {
    // TUI only: RPC/JSON/print modes are headless (no PTY shepr can display),
    // and RPC still reports hasUI=true, so mode is the reliable gate.
    if (ctx?.mode !== "tui") {
      return;
    }
    rootSession = true;
    updateSessionRef(ctx);
    await reportSession(event?.reason);
    // A reload can replace this extension mid-run without emitting another agent_start.
    agentActive = ctx?.isIdle?.() === false;
    publishState(true);
  });

  pi.on("agent_start", (_event, ctx) => {
    if (!rootSession) {
      return;
    }
    updateSessionRef(ctx);
    void reportSession();
    agentActive = true;
    publishState();
  });

  pi.on("agent_settled", (_event, ctx) => {
    if (!rootSession || ctx?.isIdle?.() !== true) {
      return;
    }

    agentActive = false;
    publishState();
  });
}
