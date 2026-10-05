import { afterEach, expect, test } from "bun:test";
import { rm } from "node:fs/promises";
import { createServer, type Server } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expectContractTrace } from "../contract_traces.ts";

const originalArgv = process.argv;
const originalEnvironment = {
  SHEPR_ENV: process.env.SHEPR_ENV,
  SHEPR_BUILD_PROFILE: process.env.SHEPR_BUILD_PROFILE,
  SHEPR_PANE_ID: process.env.SHEPR_PANE_ID,
  SHEPR_SOCKET_PATH: process.env.SHEPR_SOCKET_PATH,
  OMPCODE: process.env.OMPCODE,
};

let server: Server | undefined;
let socketPath: string | undefined;
let importCounter = 0;

afterEach(async () => {
  await new Promise<void>((resolve, reject) => {
    if (!server) {
      resolve();
      return;
    }
    server.close((error) => (error ? reject(error) : resolve()));
  });
  server = undefined;

  if (socketPath) {
    await rm(socketPath, { force: true });
    socketPath = undefined;
  }

  process.argv = originalArgv;
  for (const [name, value] of Object.entries(originalEnvironment)) {
    if (value === undefined) {
      delete process.env[name];
    } else {
      process.env[name] = value;
    }
  }
});

const integrations = [
  { name: "Pi", modulePath: "./pi/shepr-agent-state.ts" },
  { name: "Oh My Pi", modulePath: "./omp/shepr-agent-state.ts" },
] as const;

function importFresh(modulePath: string) {
  importCounter += 1;
  return import(`${modulePath}?test=${importCounter}`);
}

type Handler = (event: unknown, context: unknown) => unknown;

function createExtensionHarness() {
  const handlers = new Map<string, Handler>();
  const eventHandlers = new Map<string, Handler>();
  return {
    handlers,
    eventHandlers,
    pi: {
      on(event: string, handler: Handler) {
        handlers.set(event, handler);
      },
      events: {
        on(event: string, handler: Handler) {
          eventHandlers.set(event, handler);
          return () => {};
        },
      },
    },
  };
}

function configureIntegrationEnvironment(recordingSocketPath: string) {
  // Tests may run inside an OMP shell; nested-session cases opt in explicitly.
  delete process.env.OMPCODE;
  process.env.SHEPR_BUILD_PROFILE = "release";
  process.env.SHEPR_ENV = "1";
  process.env.SHEPR_SOCKET_PATH = recordingSocketPath;
  process.env.SHEPR_PANE_ID = "test:p1";
}

async function startRecordingServer(name: string): Promise<unknown[]> {
  const recordingSocketPath = join(tmpdir(), `shepr-${name}-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });

  const requests: unknown[] = [];
  const recordingServer = createServer((socket) => {
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline === -1) {
        return;
      }
      requests.push(JSON.parse(input.slice(0, newline)));
      socket.end("{}\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(recordingSocketPath, resolve);
  });
  configureIntegrationEnvironment(recordingSocketPath);
  return requests;
}

test("OpenCode stays disabled without the Shepr socket environment", async () => {
  process.env.SHEPR_BUILD_PROFILE = "release";
  process.env.SHEPR_ENV = "1";
  process.env.SHEPR_PANE_ID = "test:p1";
  delete process.env.SHEPR_SOCKET_PATH;

  const { SheprAgentStatePlugin } = await importFresh("./opencode/shepr-agent-state.js");

  expect(await SheprAgentStatePlugin()).toEqual({});
});

for (const integration of integrations) {
  test(`${integration.name} reload preserves working state when the agent is active`, async () => {
    const requests = await startRecordingServer(
      integration.name.toLowerCase().replaceAll(" ", "-"),
    );
    const { handlers, pi } = createExtensionHarness();

    const { default: install } = await importFresh(integration.modulePath);
    install(pi);

    const sessionStart = handlers.get("session_start");
    expect(sessionStart).toBeDefined();
    await sessionStart?.(
      { reason: "reload" },
      {
        hasUI: true,
        mode: "tui",
        isIdle: () => false,
        sessionManager: {
          getSessionFile: () => undefined,
          getSessionId: () => "integration-session",
        },
      },
    );

    const reportedState = () => {
      for (const request of requests) {
        if (!isRecord(request) || request.method !== "pane.report_agent") {
          continue;
        }
        const params = request.params;
        if (isRecord(params) && typeof params.state === "string") {
          return params.state;
        }
      }
      return undefined;
    };

    const deadline = Date.now() + 1_000;
    while (Date.now() < deadline && reportedState() === undefined) {
      await Bun.sleep(5);
    }

    expect(reportedState()).toBe("working");
  });
}

test("OMP ignores nested sessions launched inside another OMP shell", async () => {
  const requests = await startRecordingServer("omp-nested");
  process.env.OMPCODE = "1";
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./omp/shepr-agent-state.ts");
  install(pi);

  // OMP sets `OMPCODE` on every shell it spawns. A nested `omp` inherits it and
  // must not claim the pane's session for its short-lived conversation.
  expect(handlers.size).toBe(0);
  await handlers.get("session_start")?.(
    { reason: "startup" },
    {
      hasUI: true,
      isIdle: () => true,
      sessionManager: {
        getSessionFile: () => "/tmp/omp-nested.jsonl",
        getSessionId: () => "omp-nested",
      },
    },
  );
  await Bun.sleep(25);

  expect(requests).toEqual([]);
});

test("Pi reports idle only after the agent settles", async () => {
  const requests = await startRecordingServer("pi-settled");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/shepr-agent-state.ts");
  install(pi);

  expect(completionHandlers(handlers)).toEqual(["agent_settled"]);
  let idle = true;
  const context = piContext(() => idle);
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => requestStates(requests).length === 1);

  idle = false;
  handlers.get("agent_start")?.({}, context);
  await waitFor(() => requestStates(requests).length === 2);
  expect(requestStates(requests)).toEqual(["idle", "working"]);
  expect(handlers.has("agent_end")).toBe(false);

  const requestCountBeforeStaleSettlement = requests.length;
  handlers.get("agent_settled")?.({}, context);
  await Bun.sleep(25);
  expect(requests).toHaveLength(requestCountBeforeStaleSettlement);
  expect(requestStates(requests)).toEqual(["idle", "working"]);

  idle = true;
  handlers.get("agent_settled")?.({}, context);
  await waitFor(() => requestStates(requests).length === 3);
  expect(requestStates(requests)).toEqual(["idle", "working", "idle"]);
});

test("Pi does not report state without a session reference", async () => {
  const requests = await startRecordingServer("pi-sessionless-state");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/shepr-agent-state.ts");
  install(pi);

  const context = {
    ...piContext(() => false),
    sessionManager: {
      getSessionFile: () => undefined,
      getSessionId: () => undefined,
    },
  };
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  handlers.get("agent_start")?.({}, context);
  await Bun.sleep(25);

  expect(requests).toEqual([]);
});

test("Pi ignores RPC sessions even when UI APIs are available", async () => {
  const requests = await startRecordingServer("pi-rpc");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/shepr-agent-state.ts");
  install(pi);

  const context = {
    ...piContext(() => true),
    hasUI: true,
    mode: "rpc",
  };
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  handlers.get("agent_start")?.({}, context);
  handlers.get("agent_settled")?.({}, context);
  await Bun.sleep(25);

  expect(requests).toEqual([]);
});

test("Pi settlement preserves explicit blocked-state precedence", async () => {
  const requests = await startRecordingServer("pi-settled-blocked");
  const { eventHandlers, handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/shepr-agent-state.ts");
  install(pi);

  let idle = true;
  const context = piContext(() => idle);
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => requestStates(requests).length === 1);
  idle = false;
  handlers.get("agent_start")?.({}, context);
  await waitFor(() => requestStates(requests).length === 2);
  eventHandlers.get("shepr:blocked")?.({ active: true, label: "approval" }, context);
  await waitFor(() => requestStates(requests).length === 3);

  idle = true;
  handlers.get("agent_settled")?.({}, context);
  await Bun.sleep(25);
  expect(requestStates(requests)).toEqual(["idle", "working", "blocked"]);

  eventHandlers.get("shepr:blocked")?.({ active: false }, context);
  await waitFor(() => requestStates(requests).length === 4);
  expect(requestStates(requests)).toEqual(["idle", "working", "blocked", "idle"]);
});

test("Pi deduplicates blocked state when prompt labels change", async () => {
  const requests = await startRecordingServer("pi-blocked-dedup");
  const { eventHandlers, handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/shepr-agent-state.ts");
  install(pi);

  const context = piContext(() => true);
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => requestStates(requests).length === 1);

  eventHandlers.get("shepr:blocked")?.({ active: true, label: "first approval" }, context);
  await waitFor(() => requestStates(requests).length === 2);
  eventHandlers.get("shepr:blocked")?.({ active: true, label: "second approval" }, context);
  await Bun.sleep(25);
  expect(requestStates(requests)).toEqual(["idle", "blocked"]);

  eventHandlers.get("shepr:blocked")?.({ active: false }, context);
  await Bun.sleep(25);
  expect(requestStates(requests)).toEqual(["idle", "blocked"]);
  eventHandlers.get("shepr:blocked")?.({ active: false }, context);
  await waitFor(() => requestStates(requests).length === 3);
  expect(requestStates(requests)).toEqual(["idle", "blocked", "idle"]);
});

test("Pi reports the session replacement source", async () => {
  const requests = await startRecordingServer("pi-session-source");
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./pi/shepr-agent-state.ts");
  install(pi);

  const sessionStart = handlers.get("session_start");
  expect(sessionStart).toBeDefined();
  await sessionStart?.(
    { reason: "new" },
    {
      hasUI: true,
      mode: "tui",
      isIdle: () => true,
      sessionManager: {
        getSessionFile: () => "/tmp/pi-new.jsonl",
        getSessionId: () => "pi-new",
      },
    },
  );

  const reportedSession = () =>
    requests.find((request) => isRecord(request) && request.method === "pane.report_agent_session");
  const deadline = Date.now() + 1_000;
  while (Date.now() < deadline && reportedSession() === undefined) {
    await Bun.sleep(5);
  }

  const request = reportedSession();
  expect(request).toBeDefined();
  expect(isRecord(request) && isRecord(request.params) ? request.params.session_start_source : null)
    .toBe("new");
});

test("Pi serializes its agent-start session report before its working state", async () => {
  const recordingSocketPath = join(tmpdir(), `shepr-pi-session-order-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });

  const requests: unknown[] = [];
  let acknowledgeSessionReport: (() => void) | undefined;
  const recordingServer = createServer((socket) => {
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline === -1) {
        return;
      }
      const request = JSON.parse(input.slice(0, newline));
      requests.push(request);
      if (isRecord(request) && request.method === "pane.report_agent_session") {
        acknowledgeSessionReport = () => socket.end("{}\n");
        return;
      }
      socket.end("{}\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(recordingSocketPath, resolve);
  });

  configureIntegrationEnvironment(recordingSocketPath);
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/shepr-agent-state.ts");
  install(pi);

  let idle = true;
  const context = {
    ...piContext(() => idle),
    sessionManager: {
      getSessionFile: () => "/tmp/pi-new.jsonl",
      getSessionId: () => "pi-new",
    },
  };
  const sessionStart = handlers.get("session_start");
  expect(sessionStart).toBeDefined();
  const sessionStartResult = sessionStart?.(
    { reason: "new" },
    context,
  );

  const deadline = Date.now() + 1_000;
  while (Date.now() < deadline && acknowledgeSessionReport === undefined) {
    await Bun.sleep(5);
  }
  expect(acknowledgeSessionReport).toBeDefined();
  expect(
    requests.some((request) => isRecord(request) && request.method === "pane.report_agent"),
  ).toBe(false);

  const acknowledgeStartup = acknowledgeSessionReport;
  acknowledgeSessionReport = undefined;
  acknowledgeStartup?.();
  await sessionStartResult;

  const stateDeadline = Date.now() + 1_000;
  while (
    Date.now() < stateDeadline &&
    !requests.some((request) => isRecord(request) && request.method === "pane.report_agent")
  ) {
    await Bun.sleep(5);
  }
  expect(requests.map((request) => (isRecord(request) ? request.method : undefined))).toEqual([
    "pane.report_agent_session",
    "pane.report_agent",
  ]);

  idle = false;
  handlers.get("agent_start")?.({}, context);
  const agentStartDeadline = Date.now() + 1_000;
  while (Date.now() < agentStartDeadline && acknowledgeSessionReport === undefined) {
    await Bun.sleep(5);
  }
  expect(acknowledgeSessionReport).toBeDefined();
  await Bun.sleep(25);
  expect(requests.map((request) => (isRecord(request) ? request.method : undefined))).toEqual([
    "pane.report_agent_session",
    "pane.report_agent",
    "pane.report_agent_session",
  ]);
  expect(requestStates(requests)).toEqual(["idle"]);

  const acknowledgeAgentStart = acknowledgeSessionReport;
  acknowledgeSessionReport = undefined;
  acknowledgeAgentStart?.();
  const workingDeadline = Date.now() + 1_000;
  while (Date.now() < workingDeadline && requestStates(requests).length < 2) {
    await Bun.sleep(5);
  }
  expect(requests.map((request) => (isRecord(request) ? request.method : undefined))).toEqual([
    "pane.report_agent_session",
    "pane.report_agent",
    "pane.report_agent_session",
    "pane.report_agent",
  ]);
  expect(requestStates(requests)).toEqual(["idle", "working"]);
  expect(requests.some(requestHasMessage)).toBe(false);
  const sequences = requests.map(requestSeq);
  for (let index = 1; index < sequences.length; index += 1) {
    expect(sequences[index]).toBe((sequences[index - 1] as number) + 1);
  }
  expectContractTrace("pi", requests);
});

async function startDroppedFirstResponseServer(name: string) {
  const recordingSocketPath = join(tmpdir(), `shepr-${name}-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });

  let connectionCount = 0;
  let droppedStateResponse = false;
  const attemptedRequests: unknown[] = [];
  const deliveredRequests: unknown[] = [];
  const recordingServer = createServer((socket) => {
    connectionCount += 1;
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline === -1) {
        return;
      }
      const request = JSON.parse(input.slice(0, newline));
      attemptedRequests.push(request);
      if (
        !droppedStateResponse &&
        isRecord(request) &&
        request.method === "pane.report_agent"
      ) {
        droppedStateResponse = true;
        return;
      }
      deliveredRequests.push(request);
      socket.end("{}\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(recordingSocketPath, resolve);
  });

  configureIntegrationEnvironment(recordingSocketPath);
  return {
    attemptedRequests,
    deliveredRequests,
    connectionCount: () => connectionCount,
  };
}

test("Oh My Pi retries working before a queued idle state", async () => {
  const { attemptedRequests } = await startDroppedFirstResponseServer("omp-retry");
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./omp/shepr-agent-state.ts");
  install(pi, { idleDebounceMs: 0 });

  const context = {
    hasUI: true,
    isIdle: () => false,
    sessionManager: {
      getSessionFile: () => undefined,
      getSessionId: () => "omp-retry-session",
    },
  };
  handlers.get("session_start")?.({ reason: "startup" }, context);
  handlers.get("agent_end")?.({ messages: [] }, context);

  const deadline = Date.now() + 2_500;
  const stateAttempts = () => attemptedRequests.filter(
    (request) => isRecord(request) && request.method === "pane.report_agent",
  );
  while (Date.now() < deadline && stateAttempts().length < 3) {
    await Bun.sleep(5);
  }

  expect(requestStates(stateAttempts())).toEqual(["working", "working", "idle"]);
  expect(stateAttempts()[1]).toEqual(stateAttempts()[0]);
});

test("Oh My Pi keeps working when a turn ends with a scheduled continuation", async () => {
  const requests = await startRecordingServer("omp-will-continue");
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./omp/shepr-agent-state.ts");
  install(pi, { idleDebounceMs: 0 });

  let idle = true;
  const context = {
    hasUI: true,
    isIdle: () => idle,
    sessionManager: {
      getSessionFile: () => undefined,
      getSessionId: () => "omp-continuation-session",
    },
  };

  handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => requestStates(requests).length === 1);

  idle = false;
  handlers.get("agent_start")?.({}, context);
  await waitFor(() => requestStates(requests).length === 2);
  expect(requestStates(requests)).toEqual(["idle", "working"]);

  // OMP already scheduled an automatic continuation, so this loop end is not a
  // user-visible settle and must not publish idle.
  handlers.get("agent_end")?.({ messages: [], willContinue: true }, context);
  await Bun.sleep(50);
  expect(requestStates(requests)).toEqual(["idle", "working"]);

  // The real terminal end still settles the pane.
  idle = true;
  handlers.get("agent_end")?.({ messages: [] }, context);
  await waitFor(() => requestStates(requests).length === 3);
  expect(requestStates(requests)).toEqual(["idle", "working", "idle"]);
});

test("Oh My Pi does not report state without a session reference", async () => {
  const requests = await startRecordingServer("omp-sessionless-state");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./omp/shepr-agent-state.ts");
  install(pi);

  const context = {
    hasUI: true,
    isIdle: () => false,
    sessionManager: {
      getSessionFile: () => undefined,
      getSessionId: () => undefined,
    },
  };
  handlers.get("session_start")?.({ reason: "startup" }, context);
  handlers.get("agent_start")?.({}, context);
  await Bun.sleep(25);

  expect(requests).toEqual([]);
});

test("Oh My Pi reports session-bound state", async () => {
  const requests = await startRecordingServer("omp-contract");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./omp/shepr-agent-state.ts");
  install(pi, { idleDebounceMs: 0 });

  handlers.get("session_start")?.(
    { reason: "startup" },
    {
      hasUI: true,
      isIdle: () => false,
      sessionManager: {
        getSessionFile: () => undefined,
        getSessionId: () => "omp-contract-session",
      },
    },
  );
  await waitFor(() => requests.length >= 2);
  expect(requestStates(requests)).toEqual(["working"]);
  expect(requests.some(requestHasMessage)).toBe(false);
  expectContractTrace("omp", requests);
});

test("Oh My Pi forwards the session-start reason when the agent supplies it", async () => {
  const requests = await startRecordingServer("omp-session-start-reason");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./omp/shepr-agent-state.ts");
  install(pi);

  handlers.get("session_start")?.(
    { reason: "fork" },
    {
      hasUI: true,
      isIdle: () => false,
      sessionManager: {
        getSessionFile: () => undefined,
        getSessionId: () => "omp-fork-session",
      },
    },
  );
  await waitFor(() => requests.length >= 1);

  const first = requests[0];
  expect(isRecord(first) && isRecord(first.params) ? first.params.session_start_source : null).toBe(
    "fork",
  );
});

test("Oh My Pi deduplicates blocked state when prompt labels change", async () => {
  const requests = await startRecordingServer("omp-blocked-dedup");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./omp/shepr-agent-state.ts");
  install(pi);

  const context = {
    hasUI: true,
    isIdle: () => false,
    sessionManager: {
      getSessionFile: () => undefined,
      getSessionId: () => "omp-blocked-session",
    },
  };
  handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => requestStates(requests).length === 1);

  handlers.get("tool_approval_requested")?.({ reason: "first approval" }, context);
  await waitFor(() => requestStates(requests).length === 2);
  handlers.get("tool_approval_requested")?.({ reason: "second approval" }, context);
  await Bun.sleep(25);
  expect(requestStates(requests)).toEqual(["working", "blocked"]);

  handlers.get("tool_approval_resolved")?.({}, context);
  await Bun.sleep(25);
  expect(requestStates(requests)).toEqual(["working", "blocked"]);
  handlers.get("tool_approval_resolved")?.({}, context);
  await waitFor(() => requestStates(requests).length === 3);
  expect(requestStates(requests)).toEqual(["working", "blocked", "working"]);
});

test("Pi retries working state after an unanswered socket attempt", async () => {
  const { attemptedRequests, deliveredRequests, connectionCount } =
    await startDroppedFirstResponseServer("pi-retry");
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./pi/shepr-agent-state.ts");
  install(pi);

  const sessionStart = handlers.get("session_start");
  expect(sessionStart).toBeDefined();
  await sessionStart?.(
    { reason: "startup" },
    {
      hasUI: true,
      mode: "tui",
      isIdle: () => false,
      sessionManager: {
        getSessionFile: () => undefined,
        getSessionId: () => "pi-retry-session",
      },
    },
  );

  const reportedWorking = () =>
    deliveredRequests.some((request) => {
      if (!isRecord(request) || request.method !== "pane.report_agent") {
        return false;
      }
      const params = request.params;
      return isRecord(params) && params.state === "working";
    });

  const deadline = Date.now() + 2_500;
  while (Date.now() < deadline && !reportedWorking()) {
    await Bun.sleep(5);
  }

  expect(connectionCount()).toBeGreaterThanOrEqual(2);
  const stateAttempts = attemptedRequests.filter(
    (request) => isRecord(request) && request.method === "pane.report_agent",
  );
  expect(stateAttempts.length).toBeGreaterThanOrEqual(2);
  expect(stateAttempts[1]).toEqual(stateAttempts[0]);
  expect(reportedWorking()).toBe(true);
});

function completionHandlers(handlers: Map<string, Handler>): string[] {
  return ["agent_end", "agent_settled"].filter((event) => handlers.has(event));
}

function piContext(isIdle: () => boolean) {
  return {
    hasUI: true,
    mode: "tui",
    isIdle,
    sessionManager: {
      getSessionFile: () => undefined,
      getSessionId: () => "pi-test-session",
    },
  };
}

function requestStates(requests: unknown[]): unknown[] {
  return requests
    .filter((request) => isRecord(request) && request.method === "pane.report_agent")
    .map(requestState);
}

async function waitFor(predicate: () => boolean, timeoutMs = 1_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline && !predicate()) {
    await Bun.sleep(5);
  }
  expect(predicate()).toBe(true);
}

function requestState(request: unknown): unknown {
  if (!isRecord(request) || !isRecord(request.params)) {
    return undefined;
  }
  return request.params.state;
}

function requestHasMessage(request: unknown): boolean {
  return isRecord(request) && isRecord(request.params) && "message" in request.params;
}

function requestSeq(request: unknown): unknown {
  if (!isRecord(request) || !isRecord(request.params)) {
    return undefined;
  }
  return request.params.seq;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

for (const integration of integrations) {
  for (const profile of ["dev", "unknown", undefined]) {
    test(`${integration.name} rejects ${profile ?? "missing"} pane profile`, async () => {
      configureIntegrationEnvironment("unused.sock");
      if (profile === undefined) delete process.env.SHEPR_BUILD_PROFILE;
      else process.env.SHEPR_BUILD_PROFILE = profile;
      const { handlers, pi } = createExtensionHarness();
      const { default: install } = await importFresh(integration.modulePath);
      install(pi);
      expect(handlers.size).toBe(0);
    });
  }
}
