# cones and pi

[All comparisons](README.md)

**Choose pi** for a coding harness you can customize: model providers, tools,
context handling and interface behavior. Extensions can replace tools or add
dialogs; SDK and RPC interfaces support embedding it in other applications.
Its native session tree lets you revisit conversation branches. Adopting pi
means adopting its execution behavior even when switching model vendors.
[Overview][pi-overview], [extensions][pi-extensions], [SDK][pi-sdk], [RPC][pi-rpc]

Pi omits a built-in tool-approval system and runs with its process's permissions.
Project trust controls resource loading; it does not sandbox tools. Confirmation
flows and isolation require additional configuration or extensions. cones adds
no permission layer. [Pi security][pi-security], [cones boundary][cones-architecture]

**Add cones** on macOS when several pi or mixed-harness sessions need shared
previews, project navigation and history. cones hosts pi's native terminal,
passes its launch options and delegates conversation forks to pi. Hosted
terminals survive dashboard closure, with one attachment at a time. Forks keep
the original directory, so parallel edits need separately prepared worktrees.
[cones support][cones-harness], [forks][cones-dashboard]

Support is partial: cones cannot join external pi terminals, and several pi
processes in one directory prevent reliable transcript attribution. Pi dialogs
have no input-request signal in cones. Native message delivery and supervised
pi jobs are unsupported. Use pi alone when its terminal and session tree cover
your work; add cones for visibility across sessions with those limits understood.
[Integration limits][cones-harness]

Reviewed 2026-09-22. Pi: personal branch [61716b03][pi-snapshot] over upstream
[853a80d2][pi-upstream], plus v0.87.1 documentation. The personal Bedrock fix is
not credited upstream; extension APIs changed after the snapshot.
[Release notes][pi-changelog]. [Review basis](README.md#review-basis).

[pi-overview]: https://github.com/earendil-works/pi/blob/853a80d26c90a14c1886f0ebb8ffaae133ca2185/packages/coding-agent/README.md
[pi-extensions]: https://github.com/earendil-works/pi/blob/853a80d26c90a14c1886f0ebb8ffaae133ca2185/packages/coding-agent/docs/extensions.md
[pi-sdk]: https://github.com/earendil-works/pi/blob/853a80d26c90a14c1886f0ebb8ffaae133ca2185/packages/coding-agent/docs/sdk.md
[pi-rpc]: https://github.com/earendil-works/pi/blob/853a80d26c90a14c1886f0ebb8ffaae133ca2185/packages/coding-agent/docs/rpc.md
[pi-security]: https://github.com/earendil-works/pi/blob/v0.87.1/packages/coding-agent/docs/security.md
[cones-architecture]: ../architecture.md
[cones-harness]: ../harness.md
[cones-dashboard]: ../dashboard.md
[pi-snapshot]: https://github.com/YuvalSarel1/pi/commit/61716b03c944d3096a94d0e7ab7dd666b4a370ec
[pi-upstream]: https://github.com/earendil-works/pi/commit/853a80d26c90a14c1886f0ebb8ffaae133ca2185
[pi-changelog]: https://github.com/earendil-works/pi/blob/v0.87.1/packages/coding-agent/CHANGELOG.md
