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

test("Pi reports native dialogs, nested waits, switches, failure and reload without handling input", async (t) => {
  const { default: report } = await import("../assets/harnesses/pi-report.mjs");
  const directory = mkdtempSync(join(tmpdir(), "cones-pi-report-test-"));
  const path = join(directory, "session.json");
  const original = process.env.CONES_PI_REPORT;
  process.env.CONES_PI_REPORT = path;
  const handlers = new Map();
  let tick, failed = false, idle = true, id = "aaaaaaaa-1111-4111-8111-111111111111";
  t.mock.method(globalThis, "setInterval", (fn) => { tick = fn; return { unref() {} }; });
  t.mock.method(globalThis, "clearInterval", () => {});
  const pi = { on: (event, handler) => handlers.set(event, handler) };
  const ctx = {
    cwd: directory,
    sessionManager: {
      getSessionId: () => { if (failed) throw Error("native API failed"); return id; },
      getSessionFile: () => undefined,
    },
    isIdle: () => idle,
  };
  const emit = (name) => handlers.get(name)({ type: name }, ctx);
  const read = () => JSON.parse(readFileSync(path, "utf8"));
  try {
    report(pi);
    emit("ui_prompt_start");
    tick();
    assert.equal(read().waiting, true, "a startup dialog may precede this observer's session_start");
    emit("session_start");
    assert.equal(read().waiting, true);
    emit("ui_prompt_end");
    assert.equal(read().pid, process.pid);
    assert.equal(read().session.id, id);
    assert.equal(read().idle, true);
    assert.equal(read().waiting, false);
    emit("ui_prompt_start");
    assert.equal(read().waiting, true);
    emit("ui_prompt_start"); emit("ui_prompt_end");
    assert.equal(read().waiting, true, "ending one nested dialog leaves the other pending");
    emit("ui_prompt_end");
    assert.equal(read().waiting, false);
    idle = false; emit("agent_start");
    assert.equal(read().idle, false);
    failed = true; tick();
    assert.equal(existsSync(path), false, "API failure withdraws the report");
    failed = false; tick();
    assert.equal(read().idle, false);
    utimesSync(path, new Date(0), new Date(0)); tick();
    assert.ok(statSync(path).mtimeMs > 0, "unchanged state refreshes liveness");
    emit("ui_prompt_start"); emit("session_shutdown");
    assert.equal(existsSync(path), false);
    id = "bbbbbbbb-2222-4222-8222-222222222222";
    report(pi); emit("session_start");
    assert.equal(read().session.id, id);
    assert.equal(read().waiting, false, "session replacement clears the old wait");
    assert.ok(!handlers.has("input") && !handlers.has("tool_call"));
    emit("session_shutdown");
    assert.equal(existsSync(path), false);
  } finally {
    handlers.get("session_shutdown")?.();
    if (original === undefined) delete process.env.CONES_PI_REPORT;
    else process.env.CONES_PI_REPORT = original;
    rmSync(directory, { recursive: true, force: true });
  }
});
