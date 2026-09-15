# The coordinator: one session per folder

Back to the [README](../README.md). Its row's kind is in [harness.md](harness.md#kinds), the command in [cli.md](cli.md).

Coordination between agents sharing a tree is not cones logic. It is the [start-orchestrator](https://github.com/YuvalSarel1/orchestrator) skill: one Claude Code session that finds every agent whose cwd is the folder, introduces itself, holds commits until it says go, relays findings and insists on a clean tree when the last job ends. cones owns schedule and ledger; the coordinator owns the conversation. cones ships the skill inside its binary, from `assets/coordinator/`, so nothing needs installing.

```sh
cones coordinator start            # this folder
cones coordinator start ~/src/app  # another folder
```

Each start rewrites the plugin under `~/.cones/coordinator/plugin` (or the `--state-dir`), then runs `claude --bg --plugin-dir <that> /cones:start-orchestrator` in the folder, so the coordinator is an ordinary background session loaded with the skill for that session only: it shows in `cones ls` and the dashboard, `claude attach <id>` opens it, and telling it "stop orchestrator" ends its role. The skill writes `~/.claude/orchestrator/<sha1 of the folder>.json` with its pid and peers every tick, the same file a hand-typed `/start-orchestrator` from an installed copy of the skill writes, so `cones coordinator start` and the skill's own guard both see a coordinator started either way: when that file names a live process for the folder, the command prints it and does nothing.

The dashboard reads the same file to mark the coordinator's row: a session whose pid and cwd it names says `orchestrator` where a row says `own terminal`, and its title is in cones' orange, bold, so it is told from the workers at a glance. A hand-typed coordinator in its own terminal says `orchestrator · own terminal`. The mark is that file's pid, never the session's title; `cones ls --json` carries it as `coordinator: true`. Everything else about the row follows its kind, background or interactive.

The copy under `assets/coordinator/` is the upstream skill with one line changed, the helper path, which cones fills in when it writes the plugin. Update it by copying the upstream files over and re-applying that line.
