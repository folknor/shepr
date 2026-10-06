// The agent plugins' side of the integration contract. A bun test that drives
// a plugin through a scripted session passes the requests it captured here,
// and they must equal the named trace in `contract_traces.toml`. The server
// crate's `agent_integration_contract_tests` module replays those same traces
// into terminal state, so the two halves together check what a plugin really
// sends against what the server accepts. Session ids and paths stay literal in
// comparison; the Pi trace pins a path-form resume identity. bun is a
// development dependency that the Rust tests do not have, which is why the
// traces sit in a file between them.
import { expect } from "bun:test";
import traces from "./contract_traces.toml";

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

// Seqs are clock-seeded, so a trace stores each one as its rank in the run,
// after checking the run sent them in increasing order. The request id takes
// the same `<source>:<rank>` shape; a report without a seq ranks as 0.
function normalize(requests: unknown[]): unknown[] {
  const seqs = requests.flatMap((request) =>
    isRecord(request) && isRecord(request.params) && typeof request.params.seq === "number"
      ? [request.params.seq]
      : [],
  );
  for (let index = 1; index < seqs.length; index += 1) {
    expect(seqs[index]).toBeGreaterThan(seqs[index - 1]);
  }
  return requests.map((request) => {
    if (!isRecord(request) || !isRecord(request.params)) {
      return request;
    }
    const params = { ...request.params };
    if (typeof params.seq === "number") {
      expect(request.id).toBe(`${String(params.source)}:${params.seq}`);
    } else {
      // TUI selection reports have their own clock seed and no state seq.
      expect(typeof request.id).toBe("string");
      const prefix = `${String(params.source)}:`;
      expect(String(request.id).startsWith(prefix)).toBe(true);
      expect(String(request.id).slice(prefix.length)).toMatch(/^\d+$/);
    }
    if (typeof params.seq === "number") {
      params.seq = seqs.indexOf(params.seq) + 1;
    }
    return { ...request, id: `${String(params.source)}:${params.seq ?? 0}`, params };
  });
}

export function expectContractTrace(name: string, requests: unknown[]): void {
  expect(normalize(requests)).toEqual((traces as Record<string, unknown>)[name]);
}
