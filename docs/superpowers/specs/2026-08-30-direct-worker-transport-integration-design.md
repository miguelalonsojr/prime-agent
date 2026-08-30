# Direct worker transport integration

## Goal

Integrate `tkellogg/prime-agent:feat/direct-worker-transport` into the current fork. Preserve the fork's model routing and per-subagent model and thinking-level behavior. Fix or verify the stability failures identified as directly or partially addressed in discussion #1805.

## Inputs

The integration starts from local `main` at `75d0a22bcc28494b0d1e0f0d36d7601bf27a925a`.

The latest reviewed upstream head is `PrimeIntellect-ai/prime-agent:main` at `c382f09856d4a8c8d2b765179657047d58691f25`.

The transport branch tip is `tkellogg/prime-agent:feat/direct-worker-transport` at `2902b3e007bb53bb394c46b176903a9c580504f7`. It contains these commits in order:

1. `57bbf05df04bb127734c185798f35a34ef2c120a` adds the SQLite session catalog and stabilizes agents-view refresh.
2. `2902b3e007bb53bb394c46b176903a9c580504f7` adds direct worker peer transport.

The linked discussion is [#1805](https://github.com/PrimeIntellect-ai/prime-agent/discussions/1805#discussioncomment-18208800).

## Scope

The required stability cases are:

- A burst or stall in one session does not block or corrupt other sessions.
- Agents view does not synchronously poll every session.
- A supervisor restart restores a usable active-session mapping for surviving workers. Discussions [#1870](https://github.com/PrimeIntellect-ai/prime-agent/discussions/1870) and [#1641](https://github.com/PrimeIntellect-ai/prime-agent/discussions/1641) describe this failure.
- Worker process identity and supervisor ownership remain fenced during restart and recovery. Issue [#1381](https://github.com/PrimeIntellect-ai/prime-agent/issues/1381) defines this requirement.
- A compact assistant stream gap cannot create null content blocks or crash the TUI. Issue [#648](https://github.com/PrimeIntellect-ai/prime-agent/issues/648) defines this requirement.
- Empty sessions remain deletable. Existing sessions can reopen without a CLI restart.

The reports that the branch author labels "related, but maybe not addressed" are review and test context. Their closure is not required by this integration. A finding that shows the integration causes or exposes one of those failures becomes an integration defect.

## Integration order

Work occurs on `feat/direct-worker-transport-integration` in an isolated worktree.

1. Fetch the latest `upstream/main` and the fork transport branch.
2. Merge `upstream/main` into the feature branch. Do not modify local `main`.
3. Resolve each conflict only after the user selects a resolution.
4. Run focused checks for the upstream merge.
5. Cherry-pick `57bbf05df04bb127734c185798f35a34ef2c120a`.
6. Resolve and verify its conflicts one at a time.
7. Cherry-pick `2902b3e007bb53bb394c46b176903a9c580504f7`.
8. Resolve and verify its conflicts one at a time.
9. Review semantic conflicts after both commits apply.
10. Add the minimum fixes and regression tests needed for the acceptance criteria.

A clean textual application does not prove compatibility. The review must examine changed commands, capabilities, allowlists, reconnect paths, snapshot paths, catalog invalidation, worker recovery, and model metadata flow.

## Conflict decisions

Each textual or semantic conflict is a separate user decision. The conflict report contains:

1. The affected file and behavior.
2. The current fork behavior.
3. The upstream or transport-branch behavior.
4. The available resolutions and their consequences.
5. A recommended resolution.

No conflicting behavior is removed or selected before the user decides. Mechanical defects without competing behavior can be fixed directly with a focused regression test. Such fixes remain part of the final report.

## Runtime architecture

The supervisor is the registry and lifecycle control process. It discovers workers, issues short-lived peer grants, manages worker recovery, and provides a compatibility fallback. Session data does not use the supervisor when direct transport is negotiated and authenticated.

`DaemonRoutedClient` selects the route for session-scoped commands. It obtains a direct transport ticket from the supervisor and connects to the worker socket. It falls back to supervisor routing when the peer does not advertise the capability or when direct setup fails.

The worker validates a one-use grant before accepting a direct peer. Validation covers the grant purpose, expiry, worker ID, worker process instance ID, process start identity, and socket device and inode. Restart and update paths fence grants from prior worker or supervisor generations.

The session catalog stores summary metadata in SQLite. Session JSONL files remain the durable detailed history. Agents view reads cached summaries and only refreshes detailed data when the durable source changes. A slow worker does not block list rendering.

The SQLite implementation remains behind the session catalog interface. Session behavior does not depend on SQLite-specific objects or queries outside the catalog adapter.

## Daemon protocol compatibility

The intended wire change is backward-compatible and capability-gated.

The fork and upstream assign different meanings to schema revision 23. The approved conflict resolution creates composite revision 24 with a new schema ID. Revision 24 preserves the fork protocol history and adds upstream supervisor agent-roster queries. The catalog change becomes revision 25 with `cached_session_list`. Direct peer transport becomes revision 26 with `direct_peer_transport`.

A `list` command with `refresh: false` requires schema revision 25 and `cached_session_list`. Direct transport discovery commands require schema revision 26 and `direct_peer_transport`. A client must not send these forms unless the server advertises the required schema and capability.

Older clients continue through supervisor routing and the existing list behavior. New clients connected to older daemons use the same fallback. Optional metadata must not prevent attachment, session startup, agents-view rendering, or recovery.

The integration must update the schema ID, command compatibility maps, capability declarations, and old-client/new-daemon and new-client/old-daemon tests together. Any command, event, or response shape that cannot degrade through a negotiated capability requires a separate conflict decision and a protocol-version review.

## Model routing and subagent specification

Direct transport is transport-only. It does not select, normalize, replace, or default a child model or thinking level.

The integration preserves these fork behaviors:

- executable model discovery and its cache;
- manual model eligibility and routing;
- authentication refresh for private Prime model discovery;
- explicit per-child `model` selection;
- explicit per-child `thinking` selection;
- inherited thinking when the child override is absent;
- clamping against the selected child model's capabilities;
- persistence and restoration of child model metadata.

The requested model selector, requested child thinking level, resolved model, effective clamped thinking level, and discovery or authentication error must retain their meaning through daemon requests, direct attachment, snapshots, reconnects, rehydration, and agents-view summaries.

Direct-session command allowlists must include current fork commands when those commands are valid for direct routing. `set_kernel_cwd` and all current model, thinking, and scoped-model commands require explicit review because the transport branch was created from a different history.

## Error handling and recovery

Failure to negotiate or establish direct transport falls back to supervisor transport without losing the session. Authentication failure, expired grants, reused grants, mismatched process identity, and mismatched socket identity fail closed and record a bounded diagnostic.

A direct connection failure during a mutating request must not silently repeat the mutation. Existing idempotency and uncertain-result rules remain in force across route changes.

Supervisor recovery reconstructs active-session ownership from verified worker and durable metadata. It does not bind a stale worker instance or a reused process ID. A recovered client either reattaches to the prior active-session ID or receives a structured replacement path. It does not retry an unknown ID indefinitely.

Catalog corruption or unavailability degrades locally. It does not prevent agent startup, session attachment, or interactive startup. Catalog refresh coalesces duplicate work and invalidates entries after lifecycle changes.

## Verification

Tests follow RED-GREEN-REFACTOR for behavior gaps. The integration runs only focused Vitest files, followed by the repository check command. It does not run `npm test`, `npm run build`, or `npm run dev`.

Focused verification covers:

- direct peer authentication, expiry, one-use grants, process identity, and socket identity;
- direct command authorization and supervisor fallback;
- mutation recovery across a direct connection failure;
- compact snapshot ordering and compact assistant content-index gaps;
- cached session listing, refresh coalescing, invalidation, deletion, and stale entries;
- supervisor restart with a live worker and the previous active-session ID;
- old-client/new-daemon and new-client/old-daemon combinations;
- explicit child model and thinking overrides through direct and fallback routes;
- reconnect and rehydration with persisted child model metadata;
- existing model discovery, routing eligibility, and issue 4649 regression coverage;
- empty-session deletion and session reopening;
- bounded high fan-out with a stalled or bursty worker while peer sessions and agents view remain responsive.

Every modified test file runs from `packages/coding-agent` with the repository's required focused Vitest command. `npm run check` runs from the repository root with full output. A controlled interactive smoke test covers reconnect or agents-view behavior only when automated tests cannot establish the behavior.

## Completion conditions

The integration is complete when:

- all user-selected conflict resolutions are implemented;
- the required stability and model-routing tests pass;
- every modified test file passes;
- `npm run check` reports no errors, warnings, or informational findings;
- the final diff contains only integration code, focused regressions, documentation, and required changelog fragments;
- a final whole-branch review finds no unresolved protocol, security, recovery, or model-routing defect;
- local `main` remains unchanged and no merge into `main` occurs without explicit user approval.
