# cones and Hermes Agent

[All comparisons](README.md)

**Choose Hermes** for a continuing assistant with memory, delegation, recurring
tasks and messaging access. It supplies the agent loop, tools, permissions and
provider configuration. cones supplies a workspace around supported coding
harnesses. These are different layers of choice.
[Hermes overview][h-overview], [cones architecture][c-architecture]

Hermes carries bounded memory into new sessions and loads skills on demand.
Child agents have separate conversations and can use another model. They share
the parent's directory by default; optional worktree isolation works only with
the local backend and can fall back to shared files. Interrupted children do not
resume execution after a restart. These limits matter for unattended parallel
editing. [Memory][h-memory], [delegation][h-delegation],
[worktree fallback][h-worktree-dispatch]

Its gateway scheduler can run recurring tasks and deliver results through
messaging services. Hermes supports multiple providers and local, Docker or SSH
command execution. cones has no shared agent memory or child-agent runtime;
its scheduler currently supervises only Claude Code on the local Mac, with a
timeout and permission prompts skipped. [Scheduling][h-cron],
[providers][h-providers], [execution backends][h-tools], [cones jobs][c-jobs]

**Choose cones** when finding, previewing and revisiting existing sessions across
coding harnesses is the main need. Hermes history covers its own conversations.
Using both is unsupported at the reviewed cones base: there is no Hermes adapter,
and user harness definitions are not loaded. Running Hermes in a cones shell
would provide terminal hosting only, without integrated identity, attention,
history or scheduling. That arrangement was not tested.
[cones support][c-harness], [Hermes sessions][h-sessions],
[cones registry][c-registry]

Reviewed 2026-09-22. Hermes: [3c6c132][h-base], plus official documentation;
snapshot features were not audited against a release.
[Review basis](README.md#review-basis).

[h-overview]: https://github.com/NousResearch/hermes-agent/blob/3c6c132366cf6d4650372e911d57e9b05efb1c8d/README.md
[c-architecture]: ../architecture.md
[h-memory]: https://hermes-agent.nousresearch.com/docs/user-guide/features/memory
[h-delegation]: https://hermes-agent.nousresearch.com/docs/user-guide/features/delegation
[h-worktree-dispatch]: https://github.com/NousResearch/hermes-agent/blob/3c6c132366cf6d4650372e911d57e9b05efb1c8d/tools/delegate_tool_child_run.py
[h-cron]: https://hermes-agent.nousresearch.com/docs/user-guide/features/cron
[h-providers]: https://hermes-agent.nousresearch.com/docs/integrations/providers
[h-tools]: https://hermes-agent.nousresearch.com/docs/user-guide/features/tools
[c-jobs]: ../jobs.md
[c-harness]: ../harness.md
[h-sessions]: https://github.com/NousResearch/hermes-agent/blob/3c6c132366cf6d4650372e911d57e9b05efb1c8d/website/docs/user-guide/sessions.md
[c-registry]: ../../src/harness/spec.rs
[h-base]: https://github.com/NousResearch/hermes-agent/commit/3c6c132366cf6d4650372e911d57e9b05efb1c8d
