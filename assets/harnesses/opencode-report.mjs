import { renameSync, rmSync, writeFileSync } from "node:fs";

// Loaded only by a cones-owned viewer. Read OpenCode's native TUI state without
// registering tools, changing permissions, or handling the user's input.
export default {
  id: "cones-session-report",
  async tui(api, options) {
    let previous;
    let published = 0;
    const invalidate = () => {
      previous = undefined;
      published = 0;
      try { rmSync(options.path, { force: true }); } catch {}
    };
    const publish = () => {
      try {
        if (!api.state.ready) {
          invalidate();
          return;
        }
        const route = api.route.current;
        const id = route.name === "session" ? route.params?.sessionID : undefined;
        const session = id ? api.state.session.get(id) : undefined;
        const messages = id ? api.state.session.messages(id) : [];
        const last = messages.findLast((message) =>
          message.role === "assistant" && message.tokens?.output > 0);
        const context = last?.tokens;
        const reply = last ? api.state.part(last.id).find((part) =>
          part.type === "text" && !part.synthetic && !part.ignored && part.text?.trim()) : undefined;
        const native = session ? {
          id: session.id,
          directory: session.directory,
          title: session.title,
          time: session.time,
          model: session.model,
          cost: session.cost,
          tokens: session.tokens,
        } : null;
        const report = JSON.stringify({
          version: 1,
          pid: process.pid,
          session: native,
          status: id ? api.state.session.status(id) : undefined,
          waiting: !id ? undefined
            : api.state.session.permission(id).length ? "permission"
            : api.state.session.question(id).length ? "question"
            : undefined,
          context: context ? {
            input: context.input,
            output: context.output,
            reasoning: context.reasoning,
            cache: context.cache,
          } : undefined,
          last: reply?.text.trim().split("\n", 1)[0].slice(0, 512),
        });
        const now = Date.now();
        // An unchanged idle session still proves the reporter is alive.
        if (report === previous && now - published < 1000) return;
        const temporary = `${options.path}.tmp`;
        writeFileSync(temporary, report, { mode: 0o600 });
        renameSync(temporary, options.path);
        previous = report;
        published = now;
      } catch {
        // Reporting must not interrupt OpenCode's editor or execution.
        invalidate();
      }
    };
    const timer = setInterval(publish, 250);
    api.lifecycle.onDispose(() => {
      clearInterval(timer);
      invalidate();
    });
    publish();
  },
};
