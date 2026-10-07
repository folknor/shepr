
function agentEnabled() {
  return true;
}

function sessionStartSource(reason: unknown): string | undefined {
  if (reason === undefined || reason === null || reason === "") {
    return START.startup;
  }
  // Pi's `reload` event reinitializes extensions in the same session. It is
  // not a new start; preserve other agent-supplied values for server handling.
  if (reason === "reload") {
    return undefined;
  }
  return typeof reason === "string" ? reason : undefined;
}

// Pi's agent_settled event supplies the state boundary, so it does not need
// OMP's state debounce or retry grace.
export default function (pi) {
  if (!enabled()) {
    return;
  }

  let agentActive = false;
  let lastState: AgentState | undefined;
  let rootSession = false;

  function desiredState() {
    return agentActive ? STATE.working : STATE.idle;
  }

  function publishState(force = false) {
    const next = desiredState();
    if (!force && next === lastState) {
      return;
    }
    lastState = next;
    queueState(next);
  }

  pi.on("session_start", async (event, ctx) => {
    // TUI only: RPC/JSON/print modes are headless (no PTY shepr can display),
    // and RPC still reports hasUI=true, so mode is the reliable gate.
    if (ctx?.mode !== "tui") {
      return;
    }
    rootSession = true;
    updateSessionRef(ctx);
    const startSource = sessionStartSource(event?.reason);
    if (startSource !== undefined) {
      await reportSession(startSource);
    }
    // A reload can replace this extension mid-run without emitting another agent_start.
    agentActive = ctx?.isIdle?.() === false;
    publishState(true);
  });

  pi.on("agent_start", (_event, ctx) => {
    if (!rootSession) {
      return;
    }
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
