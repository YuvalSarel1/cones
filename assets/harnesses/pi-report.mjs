import { renameSync, rmSync, writeFileSync } from "node:fs";

// A private observer loaded on cones-owned native Pi clients. It registers no
// tools or input handlers and never changes a dialog's answer.
export default function report(pi) {
  const path = process.env.CONES_PI_REPORT;
  if (!path) return;
  let context, waiting = 0, timer;
  const invalidate = () => { try { rmSync(path, { force: true }); } catch {} };
  const publish = () => {
    try {
      if (!context) { invalidate(); return; }
      const value = {
        version: 1, pid: process.pid,
        session: { id: context.sessionManager.getSessionId(), directory: context.cwd,
          file: context.sessionManager.getSessionFile() ?? null },
        waiting: waiting > 0, idle: context.isIdle(),
      };
      writeFileSync(`${path}.tmp`, JSON.stringify(value), { mode: 0o600 });
      renameSync(`${path}.tmp`, path);
    } catch { invalidate(); }
  };
  timer = setInterval(publish, 1000); timer.unref?.();
  pi.on("session_start", (_event, ctx) => {
    context = ctx;
    clearInterval(timer); timer = setInterval(publish, 1000); timer.unref?.(); publish();
  });
  pi.on("ui_prompt_start", (_event, ctx) => { context = ctx; waiting++; publish(); });
  pi.on("ui_prompt_end", (_event, ctx) => { context = ctx; waiting = Math.max(0, waiting - 1); publish(); });
  pi.on("agent_start", (_event, ctx) => { context = ctx; publish(); });
  pi.on("agent_settled", (_event, ctx) => { context = ctx; publish(); });
  pi.on("session_shutdown", () => { clearInterval(timer); context = undefined; waiting = 0; invalidate(); });
}
