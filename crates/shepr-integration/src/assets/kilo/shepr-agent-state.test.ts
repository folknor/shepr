import { afterEach, beforeEach, expect, mock, test } from "bun:test";

const originalEnvironment = {
  SHEPR_ENV: process.env.SHEPR_ENV,
  SHEPR_BUILD_PROFILE: process.env.SHEPR_BUILD_PROFILE,
  SHEPR_PANE_ID: process.env.SHEPR_PANE_ID,
  SHEPR_SOCKET_PATH: process.env.SHEPR_SOCKET_PATH,
};

const requests: unknown[] = [];
const clients: FakeClient[] = [];
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
          queueMicrotask(() => client.emit("data"));
        },
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
  process.env.SHEPR_BUILD_PROFILE = "release";
  process.env.SHEPR_ENV = "1";
  process.env.SHEPR_SOCKET_PATH = "test.sock";
  process.env.SHEPR_PANE_ID = "test:p1";
});

afterEach(() => {
  for (const [name, value] of Object.entries(originalEnvironment)) {
    if (value === undefined) {
      delete process.env[name];
    } else {
      process.env[name] = value;
    }
  }
});

async function loadPlugin() {
  importCounter += 1;
  const { SheprAgentStatePlugin } = await import(`./shepr-agent-state.js?test=${importCounter}`);
  return SheprAgentStatePlugin();
}

test("reports session IDs from info.id and sessionID payloads", async () => {
  const plugin = await loadPlugin();

  await plugin.event({
    event: { type: "session.created", properties: { info: { id: "info-session" } } },
  });
  await plugin.event({
    event: { type: "session.updated", properties: { sessionID: "field-session" } },
  });

  expect(requests.map(requestMethod)).toEqual([
    "pane.report_agent_session",
    "pane.report_agent_session",
  ]);
  expect(requests.map((request) => requestParam(request, "agent_session_id"))).toEqual([
    "info-session",
    "field-session",
  ]);
  expect(requests.map((request) => requestParam(request, "session_start_source"))).toEqual([
    "startup",
    "startup",
  ]);
  expect(requestSeq(requests[1])).toBe((requestSeq(requests[0]) as number) + 1);
});

test("routes child permission events to the root session with either ID shape", async () => {
  const plugin = await loadPlugin();

  await plugin.event({
    event: {
      type: "session.created",
      properties: { info: { id: "child-session", parentID: "root-session" } },
    },
  });
  await plugin.event({
    event: { type: "permission.asked", properties: { sessionID: "child-session" } },
  });

  expect(requests.map(requestMethod)).toEqual([
    "pane.report_agent",
  ]);
  expect(requests.map((request) => requestParam(request, "agent_session_id"))).toEqual([
    "root-session",
  ]);
  expect(requests.map((request) => requestParam(request, "state"))).toEqual(["blocked"]);
});

function requestSeq(request: unknown): unknown {
  return requestParam(request, "seq");
}

function requestMethod(request: unknown): unknown {
  return isRecord(request) ? request.method : undefined;
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
