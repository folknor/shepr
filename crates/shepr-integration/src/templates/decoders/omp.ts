
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
const DEFAULT_IDLE_DEBOUNCE_MS = @OMP_IDLE_MS@;
const DEFAULT_RETRY_GRACE_MS = @OMP_GRACE_MS@;
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

function sessionStartSource(reason: unknown, fallback: string): string | undefined {
  if (reason === undefined || reason === null || reason === "") {
    return fallback;
  }
  if (reason === "reload") {
    return undefined;
  }
  return typeof reason === "string" ? reason : undefined;
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

  function activateRootSession(ctx: any, startSource?: string): boolean {
    if (ctx?.hasUI !== true) {
      return false;
    }
    rootSession = true;
    updateSessionRef(ctx);
    if (startSource !== undefined) {
      void reportSession(startSource);
    }
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
    // A bare session_start marks startup. A `reload` reinitializes extensions
    // in the same session, so it updates state without a session report; any
    // other agent-supplied reason is forwarded for the server to judge.
    if (!activateRootSession(ctx, sessionStartSource(event?.reason, START.startup))) {
      return;
    }
    // A reload can replace this extension mid-run without emitting another agent_start.
    agentActive = ctx?.isIdle?.() === false;
    publishState(true);
  });

  pi.on("session_switch", (event, ctx) => {
    // A source-less session_switch is a resume of the selected root.
    if (!activateRootSession(ctx, sessionStartSource(event?.reason, START.resume))) {
      return;
    }
    resetSessionState();
    publishState(true);
  });

  pi.on("agent_start", (_event, ctx) => {
    if (!rootSession && !activateRootSession(ctx, START.startup)) {
      return;
    }
    clearPendingTimers();
    clearFailureState();
    agentActive = true;
    publishState();
  });

  pi.on("tool_approval_requested", (_event, ctx) => {
    if (!rootSession && !activateRootSession(ctx, START.startup)) {
      return;
    }
    activateBlocked();
  });

  pi.on("tool_approval_resolved", (_event, ctx) => {
    if (!rootSession && !activateRootSession(ctx, START.startup)) {
      return;
    }
    deactivateBlocked();
  });

  pi.on("tool_execution_start", (event, ctx) => {
    if (event?.toolName !== "ask") {
      return;
    }
    if (!rootSession && !activateRootSession(ctx, START.startup)) {
      return;
    }
    activateBlocked();
  });

  pi.on("tool_execution_end", (event, ctx) => {
    if (event?.toolName !== "ask") {
      return;
    }
    if (!rootSession && !activateRootSession(ctx, START.startup)) {
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
