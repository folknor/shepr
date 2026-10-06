import { afterEach, beforeEach, expect, mock, test } from "bun:test";
import { expectContractTrace } from "../../contract_traces.ts";
import { restoreEnvironment, saveEnvironment } from "../../bun_test_support.ts";

const originalArgv = process.argv;
const originalEnvironment = saveEnvironment([
  "SHEPR_ENV", "SHEPR_BUILD_PROFILE", "SHEPR_SOCKET_PATH", "SHEPR_PANE_ID",
]);
afterEach(() => {
  process.argv = originalArgv;
  restoreEnvironment(originalEnvironment);
});

const requests: unknown[] = [];
const clients: FakeClient[] = [];
const requestWaiters: Array<() => void> = [];
let autoAcknowledge = true;
let importCounter = 0;

type FakeClient = {
  emit: (event: string) => void;
};

mock.module("node:net", () => ({
  default: {
    createConnection(_path: string, onConnect: () => void) {
      const handlers = new Map<string, () => void>();
      const client = {
        write(input: string) {
          requests.push(JSON.parse(input.trim()));
          requestWaiters.shift()?.();
          if (autoAcknowledge) {
            queueMicrotask(() => client.emit("data"));
          }
        },
        setTimeout() {},
        on(event: string, handler: () => void) {
          handlers.set(event, handler);
        },
        destroy() {},
        emit(event: string) {
          handlers.get(event)?.();
        },
      };
      clients.push(client);
      queueMicrotask(onConnect);
      return client;
    },
  },
}));

beforeEach(() => {
  requests.length = 0;
  clients.length = 0;
  requestWaiters.length = 0;
  autoAcknowledge = true;
  process.argv = ["bun", "/$bunfs/root/src/index.js", "run"];
  process.env.SHEPR_BUILD_PROFILE = "release";
  process.env.SHEPR_ENV = "1";
  process.env.SHEPR_SOCKET_PATH = "test.sock";
  process.env.SHEPR_PANE_ID = "test:p1";
});

async function loadPlugin() {
  importCounter += 1;
  const { SheprAgentStatePlugin } = await import(`./shepr-agent-state.js?test=${importCounter}`);
  return SheprAgentStatePlugin();
}

function waitForNextRequest(): Promise<void> {
  return new Promise((resolve) => requestWaiters.push(resolve));
}

test("serializes lifecycle reports", async () => {
  autoAcknowledge = false;
  const plugin = await loadPlugin();
  const firstDispatched = waitForNextRequest();
  const working = plugin.event({
    event: {
      type: "session.status",
      properties: { sessionID: "root-session", status: { type: "busy" } },
    },
  });
  await firstDispatched;

  const secondDispatched = waitForNextRequest();
  const idle = plugin.event({
    event: {
      type: "session.status",
      properties: { sessionID: "root-session", status: { type: "idle" } },
    },
  });
  expect(clients).toHaveLength(1);

  clients[0]?.emit("data");
  await secondDispatched;
  expect(clients).toHaveLength(2);
  clients[1]?.emit("data");
  await Promise.all([working, idle]);

  expect(requests.map(requestState)).toEqual(["working", "idle"]);
  const sequences = requests.map(requestSeq);
  expect(sequences[0]).toEqual(expect.any(Number));
  expect(sequences[1]).toBe((sequences[0] as number) + 1);
});

test("suppresses redundant same-session updates", async () => {
  const plugin = await loadPlugin();

  await plugin.event({
    event: {
      type: "session.status",
      properties: { sessionID: "root-session", status: { type: "busy" } },
    },
  });
  await plugin.event({
    event: { type: "session.updated", properties: { sessionID: "root-session" } },
  });
  await plugin.event({
    event: { type: "session.updated", properties: { sessionID: "replacement-session" } },
  });

  expect(requests.map(requestMethod)).toEqual([
    "pane.report_agent",
    "pane.report_agent_session",
  ]);
  expect(requests.map(requestSessionID)).toEqual(["root-session", "replacement-session"]);
});

test("does not classify server activity in another root session as a selection", async () => {
  const plugin = await loadPlugin();

  await plugin["chat.message"]({ sessionID: "visible-session" });
  await plugin["chat.message"]({ sessionID: "attached-client-session" });

  expect(requests.map(requestMethod)).toEqual([
    "pane.report_agent_session",
    "pane.report_agent",
    "pane.report_agent",
  ]);
  expect(requests.map(requestSessionID)).toEqual([
    "visible-session",
    "visible-session",
    "attached-client-session",
  ]);
});

test("does not classify server-global root creation as a local selection", async () => {
  const plugin = await loadPlugin();

  await plugin.event({
    event: { type: "session.created", properties: { info: { id: "attached-session" } } },
  });
  await plugin.event({
    event: { type: "session.updated", properties: { sessionID: "attached-session" } },
  });

  expect(requests).toEqual([]);
});

test("anchors the local root from the chat hook across both session event shapes", async () => {
  const plugin = await loadPlugin();

  await plugin.event({
    event: { type: "session.created", properties: { info: { id: "local-session" } } },
  });
  await plugin.event({
    event: {
      type: "session.updated",
      properties: { sessionID: "local-session" },
    },
  });
  expect(requests).toEqual([]);

  await plugin["chat.message"]({ sessionID: "local-session" });

  expect(requests.map(requestMethod)).toEqual([
    "pane.report_agent_session",
    "pane.report_agent",
  ]);
  expect(requests.map(requestSessionID)).toEqual(["local-session", "local-session"]);
  expect(requestParam(requests[0], "session_start_source")).toBe("startup");
  expect(requestSeq(requests[1])).toBe((requestSeq(requests[0]) as number) + 1);
  expectContractTrace("opencode", requests);
});

test("re-sends the local root's recognized start on its later session events", async () => {
  const plugin = await loadPlugin();

  await plugin["chat.message"]({ sessionID: "local-session" });
  // Each report gets one attempt; a start lost here would leave the pane
  // unanchored unless a later session event sends it again.
  await plugin.event({
    event: { type: "session.updated", properties: { sessionID: "local-session" } },
  });
  await plugin.event({
    event: {
      type: "session.status",
      properties: { sessionID: "local-session", status: { type: "unrecognized" } },
    },
  });
  // A server-global session that is not this run's root is never a start.
  await plugin.event({
    event: { type: "session.updated", properties: { sessionID: "attached-session" } },
  });

  expect(requests.map(requestMethod)).toEqual([
    "pane.report_agent_session",
    "pane.report_agent",
    "pane.report_agent_session",
    "pane.report_agent_session",
    "pane.report_agent_session",
  ]);
  expect(requests.map(requestSessionID)).toEqual([
    "local-session",
    "local-session",
    "local-session",
    "local-session",
    "attached-session",
  ]);
  expect(requests.map((request) => requestParam(request, "session_start_source"))).toEqual([
    "startup",
    undefined,
    "startup",
    "startup",
    undefined,
  ]);
});

test("OpenCode does not report state without a session reference", async () => {
  const plugin = await loadPlugin();

  await plugin.event({
    event: { type: "session.status", properties: { status: { type: "busy" } } },
  });

  expect(requests).toEqual([]);
});

test("Kilo anchors a session named only by its info payload", async () => {
  importCounter += 1;
  const { SheprAgentStatePlugin } = await import(`../kilo/shepr-agent-state.js?test=${importCounter}`);
  const plugin = await SheprAgentStatePlugin();

  await plugin.event({
    event: { type: "session.created", properties: { info: { id: "kilo-session" } } },
  });

  expect(requests.map(requestMethod)).toEqual(["pane.report_agent_session"]);
  expect(requests.map(requestSessionID)).toEqual(["kilo-session"]);
  expect(requestParam(requests[0], "session_start_source")).toBe("startup");
});

test("Kilo reports state under the root session identity", async () => {
  importCounter += 1;
  const { SheprAgentStatePlugin } = await import(`../kilo/shepr-agent-state.js?test=${importCounter}`);
  const plugin = await SheprAgentStatePlugin();

  await plugin.event({
    event: { type: "session.created", properties: { info: { id: "kilo-contract-session" } } },
  });
  await plugin.event({
    event: {
      type: "session.created",
      properties: { info: { id: "kilo-contract-child", parentID: "kilo-contract-session" } },
    },
  });
  await plugin.event({
    event: { type: "permission.asked", properties: { sessionID: "kilo-contract-child" } },
  });

  expect(requests.map(requestMethod)).toEqual([
    "pane.report_agent_session",
    "pane.report_agent",
  ]);
  expect(requests.map(requestSessionID)).toEqual([
    "kilo-contract-session",
    "kilo-contract-session",
  ]);
  expectContractTrace("kilo", requests);
});

test("Kilo does not report state without a session reference", async () => {
  importCounter += 1;
  const { SheprAgentStatePlugin } = await import(`../kilo/shepr-agent-state.js?test=${importCounter}`);
  const plugin = await SheprAgentStatePlugin();

  await plugin.event({ event: { type: "permission.asked", properties: {} } });

  expect(requests).toEqual([]);
});

test("reports retry status as working", async () => {
  const plugin = await loadPlugin();

  await plugin.event({
    event: {
      type: "session.status",
      properties: { sessionID: "root-session", status: { type: "retry" } },
    },
  });

  expect(requests.map(requestMethod)).toEqual(["pane.report_agent"]);
  expect(requests.map(requestState)).toEqual(["working"]);
  expect(requests.map(requestSessionID)).toEqual(["root-session"]);
});

test("reports child prompts without replacing the root session", async () => {
  const plugin = await loadPlugin();

  await plugin.event({
    event: {
      type: "session.created",
      properties: {
        info: { id: "child-session", parentID: "root-session" },
      },
    },
  });

  for (const type of ["permission.asked", "question.asked"]) {
    await plugin.event({ event: { type, properties: { sessionID: "child-session" } } });
  }
  for (const type of ["permission.replied", "question.replied", "question.rejected"]) {
    await plugin.event({ event: { type, properties: { sessionID: "child-session" } } });
  }

  expect(requests.map(requestState)).toEqual([
    "blocked",
    "blocked",
    "working",
    "working",
    "working",
  ]);
  expect(requests.map(requestSessionID)).toEqual([
    "root-session",
    "root-session",
    "root-session",
    "root-session",
    "root-session",
  ]);
});

test("routes nested child prompts to their own root, not the last active root", async () => {
  const plugin = await loadPlugin();
  for (const info of [
    { id: "child-session", parentID: "root-session" },
    { id: "nested-session", parentID: "child-session" },
  ]) {
    await plugin.event({ event: { type: "session.created", properties: { info } } });
  }
  await plugin["chat.message"]({ sessionID: "other-root" });
  await plugin.event({
    event: { type: "permission.asked", properties: { sessionID: "nested-session" } },
  });
  await plugin.event({
    event: { type: "permission.replied", properties: { sessionID: "nested-session" } },
  });
  await plugin.event({
    event: { type: "session.idle", properties: { sessionID: "nested-session" } },
  });
  await plugin["chat.message"]({ sessionID: "nested-session" });

  const stateReports = requests.filter((request) => requestMethod(request) === "pane.report_agent");
  expect(stateReports.map(requestState)).toEqual(["working", "blocked", "working"]);
  expect(stateReports.map(requestSessionID)).toEqual([
    "other-root",
    "root-session",
    "root-session",
  ]);
});

test("only local run and Mini own server lifecycle, never shared servers or TUI workers", async () => {
  for (const args of [
    ["run"], ["run", "--session", "existing"], ["--mini"], ["--mini", "--session", "existing"],
    ["--print-logs", "--log-level", "DEBUG", "run"], ["run", "--", "--attach"],
  ]) {
    process.argv = ["bun", "/$bunfs/root/src/index.js", ...args];
    expect((await loadPlugin()).event).toBeFunction();
  }
  for (const args of [
    [], ["--session", "existing"], ["serve"], ["web"], ["attach", "http://localhost:4096"],
    ["run", "--attach", "http://localhost:4096"], ["--mini", "--attach=http://localhost:4096"],
    ["serve", "--", "--mini"],
  ]) {
    process.argv = ["bun", "/$bunfs/root/src/index.js", ...args];
    expect(await loadPlugin()).toEqual({});
  }
  process.argv = ["bun", "/$bunfs/root/src/cli/tui/worker.js"];
  expect(await loadPlugin()).toEqual({});
  expect(requests).toHaveLength(0);
});

function requestMethod(request: unknown): unknown {
  return isRecord(request) ? request.method : undefined;
}

test("dual server entrypoint keeps V1 hooks and never reports from the V2 shared server", async () => {
  const module = await import(`./shepr-agent-state.js?test=${++importCounter}`);
  expect(module.default.server).toBe(module.SheprAgentStatePlugin);
  expect(await module.default.setup({})).toBeUndefined();
  expect(requests).toHaveLength(0);
  const hooks = await module.default.server();
  await hooks["chat.message"]({ sessionID: "v1-root" });
  expect(requests.map(requestMethod)).toEqual([
    "pane.report_agent_session",
    "pane.report_agent",
  ]);
  expect(requestParam(requests[0], "session_start_source")).toBe("startup");
  expect(requestState(requests[1])).toBe("working");
});

function requestState(request: unknown): unknown {
  return requestParam(request, "state");
}

function requestSeq(request: unknown): unknown {
  return requestParam(request, "seq");
}

function requestSessionID(request: unknown): unknown {
  return requestParam(request, "agent_session_id");
}

function requestParam(request: unknown, name: string): unknown {
  if (!isRecord(request) || !isRecord(request.params)) {
    return undefined;
  }
  return request.params[name];
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

for (const profile of ["dev", "unknown", undefined]) {
  test(`release plugin rejects ${profile ?? "missing"} pane profile`, async () => {
    const previous = process.env.SHEPR_BUILD_PROFILE;
    try {
      if (profile === undefined) delete process.env.SHEPR_BUILD_PROFILE;
      else process.env.SHEPR_BUILD_PROFILE = profile;
      expect(await loadPlugin()).toEqual({});
      expect(requests).toEqual([]);
    } finally {
      if (previous === undefined) delete process.env.SHEPR_BUILD_PROFILE;
      else process.env.SHEPR_BUILD_PROFILE = previous;
    }
  });
}


test("deletion retires child ancestry before an id is reused", async () => {
  const plugin = await loadPlugin();
  await plugin.event({ event: {
    type: "session.created", properties: { info: { id: "reused", parentID: "old-root" } },
  } });
  await plugin.event({ event: {
    type: "session.deleted", properties: { info: { id: "reused", parentID: "old-root" } },
  } });
  await plugin.event({ event: {
    type: "permission.asked", properties: { sessionID: "reused" },
  } });
  expect(requests.map(requestSessionID)).toEqual(["reused"]);
});
