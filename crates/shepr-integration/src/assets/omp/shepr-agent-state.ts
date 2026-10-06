// installed by shepr
// managed by shepr; every release shepr server launch on this host rewrites this file.
// add custom hooks/plugins beside this file instead of editing it.
// @ts-nocheck

import net from "node:net";
import path from "node:path";

const SHEPR_ENV = process.env.SHEPR_ENV;
const socketPath = process.env.SHEPR_SOCKET_PATH;
const paneId = process.env.SHEPR_PANE_ID;
const source = "shepr:omp";
const METHOD_SESSION = "pane.report_agent_session";
const METHOD_STATE = "pane.report_agent";
const START = { startup: "startup", resume: "resume", select: "select" };
const SOCKET_WAIT_MS = 500;

// Only a release pane of a shepr server has anything to report to, and the
// agent's own decoder can stand the extension down further.
function enabled() {
  return (
    process.env.SHEPR_BUILD_PROFILE === "release" &&
    SHEPR_ENV === "1" &&
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

type AgentState = "working" | "blocked" | "idle";

type QueuedState = {
  state: AgentState;
  seq: number;
};

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

// OMP marks every shell it spawns with OMPCODE=1. A nested `omp` launched from
// a parent session's shell inherits it, so that process is not the pane's root
// agent and must not report its short-lived session over the parent's.
const nestedOmpSession = process.env.OMPCODE === "1";

function agentEnabled() {
  return !nestedOmpSession;
}

// These timers describe OMP state events, not socket delivery retries. OMP can
// report agent_end while an automatic provider retry is starting, so retain
// Working during the retry window and delay Idle across a quick new turn. Pi's
// separate agent_settled event already denotes settlement; other integrations
// report their own lifecycle hooks, so this is not a shared agent policy.
const DEFAULT_IDLE_DEBOUNCE_MS = 250;
const DEFAULT_RETRY_GRACE_MS = 2500;
// Status codes must be whole numbers so token counts and durations do not
// match one of their numeric suffixes.
const retryableErrorPattern =
  /overloaded|provider.?returned.?error|rate.?limit|too many requests|(?<![0-9])(?:429|500|502|503|504)(?![0-9])|service.?unavailable|server.?error|internal.?error|network.?error|connection.?error|connection.?refused|connection.?lost|websocket.?closed|websocket.?error|other side closed|fetch failed|upstream.?connect|reset before headers|socket hang up|ended without|http2 request did not get a response|timed? out|timeout|terminated|retry delay/i;

function lastAssistantMessage(messages: unknown[]): any | undefined {
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    const message = messages[i] as any;
    if (message?.role === "assistant") {
      return message;
    }
  }
  return undefined;
}

function endedOnRetryableError(event: any): boolean {
  const messages = Array.isArray(event?.messages) ? event.messages : [];
  const assistant = lastAssistantMessage(messages);
  if (assistant?.stopReason !== "error") {
    return false;
  }
  return retryableErrorPattern.test(String(assistant.errorMessage ?? ""));
}

export default function (pi, options: { idleDebounceMs?: number; retryGraceMs?: number } = {}) {
  const idleDebounceMs = options.idleDebounceMs ?? DEFAULT_IDLE_DEBOUNCE_MS;
  const retryGraceMs = options.retryGraceMs ?? DEFAULT_RETRY_GRACE_MS;
  if (!enabled()) {
    return;
  }

  let agentActive = false;
  let retryHoldActive = false;
  let failureBlocked = false;
  let blockedCount = 0;
  let lastState: AgentState | undefined;
  let idleTimer: ReturnType<typeof setTimeout> | undefined;
  let retryTimer: ReturnType<typeof setTimeout> | undefined;
  let rootSession = false;

  function clearTimer(timer: ReturnType<typeof setTimeout> | undefined) {
    if (timer) {
      clearTimeout(timer);
    }
  }

  function clearPendingTimers() {
    clearTimer(idleTimer);
    clearTimer(retryTimer);
    idleTimer = undefined;
    retryTimer = undefined;
  }

  function clearFailureState() {
    retryHoldActive = false;
    failureBlocked = false;
  }

  function desiredState() {
    if (blockedCount > 0) {
      return STATE.blocked;
    }
    if (failureBlocked) {
      return STATE.blocked;
    }
    if (agentActive || retryHoldActive) {
      return STATE.working;
    }
    return STATE.idle;
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

  function scheduleIdle() {
    clearPendingTimers();
    clearFailureState();
    idleTimer = setTimeout(() => {
      idleTimer = undefined;
      publishState();
    }, idleDebounceMs);
    idleTimer.unref?.();
  }

  function holdForRetry() {
    clearPendingTimers();
    retryHoldActive = true;
    failureBlocked = false;
    publishState();

    retryTimer = setTimeout(() => {
      retryTimer = undefined;
      retryHoldActive = false;
      failureBlocked = true;
      publishState();
    }, retryGraceMs);
    retryTimer.unref?.();
  }

  function activateRootSession(ctx: any, sessionStartSource?: string): boolean {
    if (ctx?.hasUI !== true) {
      return false;
    }
    rootSession = true;
    updateSessionRef(ctx);
    void reportSession(sessionStartSource);
    return true;
  }

  function resetSessionState() {
    clearPendingTimers();
    clearFailureState();
    agentActive = false;
    blockedCount = 0;
  }

  function activateBlocked() {
    clearPendingTimers();
    blockedCount += 1;
    publishState();
  }

  function deactivateBlocked() {
    blockedCount = Math.max(0, blockedCount - 1);
    publishState();
  }

  pi.on("session_start", (event, ctx) => {
    // Use Pi's reported reason when present; a bare session_start event marks
    // the root startup needed to establish this pane's initial session.
    if (!activateRootSession(ctx, event?.reason || START.startup)) {
      return;
    }
    // A reload can replace this extension mid-run without emitting another agent_start.
    agentActive = ctx?.isIdle?.() === false;
    publishState(true);
  });

  pi.on("session_switch", (event, ctx) => {
    // A source-less session_switch is a resume of the selected root.
    if (!activateRootSession(ctx, event?.reason || START.resume)) {
      return;
    }
    resetSessionState();
    publishState(true);
  });

  pi.on("agent_start", (_event, ctx) => {
    if (!rootSession && !activateRootSession(ctx)) {
      return;
    }
    updateSessionRef(ctx);
    void reportSession();
    clearPendingTimers();
    clearFailureState();
    agentActive = true;
    publishState();
  });

  pi.on("tool_approval_requested", (_event, ctx) => {
    if (!rootSession && !activateRootSession(ctx)) {
      return;
    }
    activateBlocked();
  });

  pi.on("tool_approval_resolved", (_event, ctx) => {
    if (!rootSession && !activateRootSession(ctx)) {
      return;
    }
    deactivateBlocked();
  });

  pi.on("tool_execution_start", (event, ctx) => {
    if (event?.toolName !== "ask") {
      return;
    }
    if (!rootSession && !activateRootSession(ctx)) {
      return;
    }
    activateBlocked();
  });

  pi.on("tool_execution_end", (event, ctx) => {
    if (event?.toolName !== "ask") {
      return;
    }
    if (!rootSession && !activateRootSession(ctx)) {
      return;
    }
    deactivateBlocked();
  });

  pi.on("agent_end", (event) => {
    if (!rootSession) {
      return;
    }
    if (!agentActive) {
      // OMP can emit duplicate/late end events while auto-retry is already
      // holding the pane in Working. Do not let an unqualified duplicate end
      // cancel the retry hold and publish a false Idle.
      return;
    }
    if (event?.willContinue === true) {
      // A continuation is already scheduled, so this end is not a settle.
      // Older builds omit the field and fall through as before.
      return;
    }

    agentActive = false;

    if (endedOnRetryableError(event)) {
      holdForRetry();
      return;
    }

    scheduleIdle();
  });

  pi.on("session_shutdown", () => {
    if (rootSession) {
      clearPendingTimers();
    }
  });
}
