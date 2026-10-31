import { readdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

// Loaded only by a cones-owned viewer. Read OpenCode's native TUI state without
// registering tools, changing permissions, or handling the user's input. Notes cones
// leaves beside the report are submitted through OpenCode's own prompt endpoint.
export default {
  id: "cones-session-report",
  // OpenCode 2's plugin shape. It takes no options from config, so the report path comes
  // through the environment. The report keeps 1.x's shape for src/opencode/reporting.rs.
  async setup(context) {
    const path = process.env.CONES_OPENCODE_REPORT;
    if (!path) return;
    const data = context.data, api = context.client;
    const shown = () => {
      const route = context.ui.router.current();
      return route.type === "session" ? route.sessionID : undefined;
    };
    let synced;
    const sync = (id) => {
      if (!id || id === synced) return;
      synced = id;
      for (const collection of [data.session, data.session.message, data.session.permission, data.session.form])
        collection.sync(id).catch(() => {});
    };
    const invalidate = () => { try { rmSync(path, { force: true }); } catch {} };
    const publish = () => {
      try {
        const id = shown();
        sync(id);
        const session = id ? data.session.get(id) : undefined;
        const last = id ? data.session.message.list(id).findLast((message) =>
          message.type === "assistant" && message.tokens?.output > 0) : undefined;
        const reply = last?.content.find((part) => part.type === "text" && part.text?.trim());
        const report = JSON.stringify({
          version: 1,
          pid: process.pid,
          session: session ? {
            id: session.id,
            directory: session.location.directory,
            title: session.title,
            time: session.time,
            model: session.model ?? last?.model,
            cost: data.session.cost(id),
            tokens: session.tokens,
          } : null,
          status: id ? { type: data.session.status(id) === "running" ? "busy" : "idle" } : undefined,
          waiting: !id ? undefined
            : data.session.permission.list(id)?.length ? "permission"
            : data.session.form.list(id)?.length ? "question"
            : undefined,
          context: last?.tokens,
          last: reply?.text.trim().split("\n", 1)[0].slice(0, 512),
        });
        writeFileSync(`${path}.tmp`, report, { mode: 0o600 });
        renameSync(`${path}.tmp`, path);
      } catch {
        // Reporting must not interrupt OpenCode's editor or execution.
        invalidate();
      }
    };
    const notes = join(dirname(path), "notes");
    const deliver = async () => {
      let names;
      try { names = readdirSync(notes); } catch { return; }
      for (const name of names) {
        if (!name.endsWith(".json")) continue;
        const file = join(notes, name), done = `${file.slice(0, -5)}.done`;
        let text, answer = {};
        try { text = readFileSync(file, "utf8"); rmSync(file); } catch { continue; }
        try {
          const note = JSON.parse(text), id = shown();
          if (!id) answer = { error: "this OpenCode shows no session" };
          else if (note.compact) await api.session.compact({ sessionID: id });
          else if (note.name !== undefined) await api.session.update({ sessionID: id, title: note.name });
          else await api.session.prompt({ sessionID: id, text: note.text });
        } catch (error) { answer = { error: String(error?.message ?? error) }; }
        try {
          writeFileSync(`${done}.tmp`, JSON.stringify(answer), { mode: 0o600 });
          renameSync(`${done}.tmp`, done);
        } catch {}
      }
    };
    const timer = setInterval(() => { deliver(); publish(); }, 250);
    publish();
    return () => { clearInterval(timer); invalidate(); };
  },
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
    // Each `<name>.json` is {text}, {name} for the session title, or {compact}, and is answered by
    // `<name>.done`, so the sender learns whether OpenCode took the note. A note that cannot be
    // removed was withdrawn by its sender, or taken by an earlier pass still awaiting
    // OpenCode, and is not delivered.
    const notes = join(dirname(options.path), "notes");
    const deliver = async () => {
      let names;
      try { names = readdirSync(notes); } catch { return; }
      for (const name of names) {
        if (!name.endsWith(".json")) continue;
        const file = join(notes, name), done = `${file.slice(0, -5)}.done`;
        let text, answer = {};
        try { text = readFileSync(file, "utf8"); rmSync(file); } catch { continue; }
        try {
          const note = JSON.parse(text);
          const route = api.route.current;
          const id = route.name === "session" ? route.params?.sessionID : undefined;
          if (!id) {
            answer = { error: "this OpenCode shows no session" };
          } else if (note.compact) {
            // What OpenCode's own /compact calls, on the model of the session's last reply.
            // It returns once the summary has replaced the context.
            const model = (message) => message.role === "assistant" && (message.providerID
              ? { providerID: message.providerID, modelID: message.modelID } : message.model);
            const last = api.state.session.messages(id).map(model).findLast((m) =>
              m?.providerID && m?.modelID);
            if (!last) {
              answer = { error: "this OpenCode session has no reply to compact yet" };
            } else {
              const result = await api.client.session.summarize({ sessionID: id, ...last });
              if (result?.error) answer = { error: JSON.stringify(result.error) };
            }
          } else if (note.name !== undefined) {
            // What OpenCode's own rename dialog calls.
            const result = await api.client.session.update({ sessionID: id, title: note.name });
            if (result?.error) answer = { error: JSON.stringify(result.error) };
          } else {
            // The native prompt endpoint: OpenCode queues it behind a running turn.
            const result = await api.client.session.promptAsync({
              sessionID: id, parts: [{ type: "text", text: note.text }],
            });
            if (result?.error) answer = { error: JSON.stringify(result.error) };
          }
        } catch (error) { answer = { error: String(error?.message ?? error) }; }
        try {
          writeFileSync(`${done}.tmp`, JSON.stringify(answer), { mode: 0o600 });
          renameSync(`${done}.tmp`, done);
        } catch {}
      }
    };
    const timer = setInterval(() => { deliver(); publish(); }, 250);
    api.lifecycle.onDispose(() => {
      clearInterval(timer);
      invalidate();
    });
    publish();
  },
};
