
function agentEnabled() {
  return true;
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
