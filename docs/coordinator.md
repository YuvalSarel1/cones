# The coordinator: one session per folder

Back to the [README](../README.md). See [harness.md](harness.md#kinds) for session kinds and [cli.md](cli.md) for the command.

The bundled start-orchestrator skill runs as one Claude Code session per folder. It discovers Claude background agents and Codex clients in that folder and its subdirectories, relays relevant findings, and orders changes to shared files. Agents with disjoint files can commit on their own green checks. Coordination is the skill's responsibility; cones launches it and displays its session.

```sh
cones coordinator start
cones coordinator start ~/src/app
```

The command first checks for a live coordinator record for the folder. If found, it prints the record and exits. Otherwise it writes the embedded plugin to `~/.cones/coordinator/plugin` (under `--state-dir` when set), then launches `claude --bg --plugin-dir <plugin> /cones:start-orchestrator` in that directory. The skill also guards against duplicates. Nothing is installed into the user's plugin directory.

The skill writes `~/.claude/orchestrator/<sha1 of the absolute folder>.json` each sweep, naming its pid, cwd and peers. The dashboard matches both pid and cwd to mark the coordinator's title orange. Without a state column, it also labels the row `orchestrator`, with `own terminal` for an interactive coordinator. JSON session output includes `coordinator: true`.

`claude attach <short id>` opens a background coordinator. Telling it "stop orchestrator" ends its coordination role and removes the status record; its background session remains until separately stopped.

The embedded copy lives under `assets/coordinator/`. When updating it from the upstream orchestrator project, retain the `__CONES_COORDINATOR_BIN__` placeholder in the skill; cones replaces it with the installed helper directory.
