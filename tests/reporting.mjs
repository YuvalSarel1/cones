import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync, rmSync, statSync, utimesSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import plugin from "../assets/harnesses/opencode-report.mjs";

test("native reporting recovers from API failure, heartbeats idle, and clears on disposal", async (t) => {
  const directory = mkdtempSync(join(tmpdir(), "cones-report-test-"));
  const path = join(directory, "session.json");
  let tick, dispose, failed = false, now = 10000;
  t.mock.method(globalThis, "setInterval", (fn) => { tick = fn; return 123; });
  const clear = t.mock.method(globalThis, "clearInterval", () => {});
  t.mock.method(Date, "now", () => now);
  const api = {
    state: {
      ready: true,
      session: {
        get: () => {
          if (failed) throw Error("native API failed");
          return { id: "ses_test", directory, title: "Task", cost: 0 };
        },
        messages: () => [],
        status: () => ({ type: "idle" }),
        permission: () => [],
        question: () => [],
      },
    },
    route: { current: { name: "session", params: { sessionID: "ses_test" } } },
    lifecycle: { onDispose: (fn) => { dispose = fn; } },
  };
  try {
    await plugin.tui(api, { path });
    const initial = readFileSync(path, "utf8");
    assert.equal(JSON.parse(initial).session.id, "ses_test");
    assert.equal(JSON.parse(initial).status.type, "idle");
    failed = true;
    tick();
    assert.equal(existsSync(path), false, "an API error withdraws the old report");
    failed = false;
    tick();
    assert.equal(readFileSync(path, "utf8"), initial, "identical state is republished after recovery");
    utimesSync(path, new Date(0), new Date(0));
    now += 1000;
    tick();
    assert.ok(statSync(path).mtimeMs > 0, "unchanged idle state refreshes liveness");
    api.state.ready = false;
    tick();
    assert.equal(existsSync(path), false);
    api.state.ready = true;
    tick();
    assert.equal(existsSync(path), true);
    dispose();
    assert.equal(clear.mock.calls.length, 1);
    assert.equal(existsSync(path), false);
  } finally {
    dispose?.();
    rmSync(directory, { recursive: true, force: true });
  }
});
