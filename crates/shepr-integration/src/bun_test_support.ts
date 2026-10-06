import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

// Keep Bun socket and config fixtures in the workspace build tree, as Rust
// tests do with shepr_test_support::ScratchDir, instead of the host temp dir.
const SCRATCH_ROOT = fileURLToPath(
  new URL("../../../target/agent-asset-tests/", import.meta.url),
);

export type SavedEnvironment = Record<string, string | undefined>;

export function saveEnvironment(names: string[]): SavedEnvironment {
  return Object.fromEntries(names.map((name) => [name, process.env[name]]));
}

export function restoreEnvironment(saved: SavedEnvironment): void {
  for (const [name, value] of Object.entries(saved)) {
    if (value === undefined) delete process.env[name];
    else process.env[name] = value;
  }
}

export async function createAssetScratchDir(prefix: string): Promise<string> {
  await mkdir(SCRATCH_ROOT, { recursive: true });
  return mkdtemp(join(SCRATCH_ROOT, `${prefix}-`));
}

export function removeAssetScratchDir(path: string): Promise<void> {
  return rm(path, { recursive: true, force: true });
}

export function assetTiming(source: string, name: string): string {
  const match = source.match(new RegExp(`^const ${name} = (.+);$`, "m"));
  if (!match) throw new Error(`asset does not define ${name}`);
  return match[1];
}

export async function flushMicrotasks(turns = 8): Promise<void> {
  for (let turn = 0; turn < turns; turn += 1) await Promise.resolve();
}

export async function nextEventLoopTurn(): Promise<void> {
  await new Promise<void>((resolve) => setImmediate(resolve));
  await flushMicrotasks();
}
