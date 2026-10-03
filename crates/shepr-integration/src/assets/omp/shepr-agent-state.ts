// installed by shepr
// managed by shepr; reinstalling or updating the integration overwrites this file.
// add custom hooks/plugins beside this file instead of editing it.
// SHEPR_INTEGRATION_ID=omp
// SHEPR_INTEGRATION_VERSION=3
// @ts-nocheck

import net from "node:net";
import path from "node:path";

const SHEPR_ENV = process.env.SHEPR_ENV;
const socketPath = process.env.SHEPR_SOCKET_PATH;
const paneId = process.env.SHEPR_PANE_ID;
const source = "shepr:omp";
// OMP marks every shell it spawns with OMPCODE=1. A nested `omp` launched from
// a parent session's shell inherits it, so that process is not the pane's root
// agent and must not report its short-lived session over the parent's.
const nestedOmpSession = process.env.OMPCODE === "1";

function enabled() {
  return process.env.SHEPR_BUILD_PROFILE === "release" && SHEPR_ENV === "1" && !!socketPath && !!paneId && !nestedOmpSession;
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

async function sendRequestNow(request: unknown): Promise<void> {
  if (await sendRequestAttempt(request, 500)) {
    return;
  }
  await sendRequestAttempt(request, 500);
}

function sendRequest(request: unknown): Promise<void> {
  requestQueue = requestQueue.then(
    () => sendRequestNow(request),
    () => sendRequestNow(request),
  );
  return requestQueue;
}

type AgentState = "working" | "blocked" | "idle";

type QueuedState = {
  state: AgentState;
  seq: number;
};

// These timers describe OMP state events, not socket delivery retries. OMP can
// report agent_end while an automatic provider retry is starting, so retain
// Working during the retry window and delay Idle across a quick new turn. Pi's
// separate agent_settled event already denotes settlement; other integrations
// report their own lifecycle hooks, so this is not a shared agent policy.
const idleDebounceMs = parseDurationEnv("SHEPR_OMP_IDLE_DEBOUNCE_MS", 250);
const retryGraceMs = parseDurationEnv("SHEPR_OMP_RETRY_GRACE_MS", 2500);
const retryableErrorPattern =
  /overloaded|provider.?returned.?error|rate.?limit|too many requests|429|500|502|503|504|service.?unavailable|server.?error|internal.?error|network.?error|connection.?error|connection.?refused|connection.?lost|websocket.?closed|websocket.?error|other side closed|fetch failed|upstream.?connect|reset before headers|socket hang up|ended without|http2 request did not get a response|timed? out|timeout|terminated|retry delay/i;
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

export function isAbsoluteSessionPath(file: unknown): file is string {
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

function parseDurationEnv(name: string, fallback: number): number {
  const raw = process.env[name];
  if (!raw) {
    return fallback;
  }
  const parsed = Number.parseInt(raw, 10);
  if (!Number.isFinite(parsed) || parsed < 0) {
    return fallback;
  }
  return parsed;
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
      agent: "omp",
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
    method: "pane.report_agent",
    params: withSessionRef({
      pane_id: paneId,
      source,
      agent: "omp",
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

export default function (pi) {
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
      return "blocked" as const;
    }
    if (failureBlocked) {
      return "blocked" as const;
    }
    if (agentActive || retryHoldActive) {
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

  pi.events.on("shepr:blocked", (data) => {
    if (!rootSession) {
      return;
    }
    if (!data?.active) {
      deactivateBlocked();
      return;
    }

    activateBlocked();
  });

  pi.on("session_start", (event, ctx) => {
    // Use Pi's reported reason when present; a bare session_start event marks
    // the root startup needed to establish this pane's initial session.
    if (!activateRootSession(ctx, event?.reason || "startup")) {
      return;
    }
    // A reload can replace this extension mid-run without emitting another agent_start.
    agentActive = ctx?.isIdle?.() === false;
    publishState(true);
  });

  pi.on("session_switch", (event, ctx) => {
    // A source-less session_switch is a resume of the selected root.
    if (!activateRootSession(ctx, event?.reason || "resume")) {
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
