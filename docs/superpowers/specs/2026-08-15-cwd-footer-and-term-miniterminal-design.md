# Cwd Footer and /term Miniterminal — Design

Date: 2026-08-15
Status: Approved design, pending implementation plan

## Problem

Prime Agent's interactive TUI gives the user no indication of the directory the
agent is operating in, and no way to change it without leaving the app. Two
features fix this:

1. A footer line at the bottom of the screen showing the current path and git
   branch.
2. A `/term` slash command that suspends the TUI into a real interactive shell;
   a `cd` performed there propagates back into the agent's working directory on
   exit.

## Scope decisions (agreed with user)

- **Which cwd propagates:** the IPython kernel cwd, footer display, and file
  autocomplete follow the user's `cd`. The session's storage/identity (session
  file location, `SessionManager` cwd) stays anchored where the session
  started. No mid-session retargeting of session storage.
- **Miniterminal shape:** suspend-and-shell. No embedded PTY overlay, no
  line-based command runner, no new native dependencies.
- **Availability:** `/term` only when the agent is idle. Busy (streaming,
  compacting, bash running, queued actions) rejects with a hint.
- **Footer content:** abbreviated path plus git branch, e.g.
  `~/projects/prime-agent (main)`.

## 1. The cwd model

A new first-class observable value: **kernel cwd** — the directory the agent's
IPython kernel process is in (`%%bash` cells and Python code execute there).

- Add optional `kernelCwd?: string` to `AgentConnectionState` (and the
  underlying snapshot in `modes/agent-connection/snapshot.ts`).
- After every ipython tool execution completes, the session runtime refreshes
  it by executing a hidden `os.getcwd()` probe through `KernelManager.execute`
  (serialized on the existing execution queue, not recorded in the session
  context, uniform across platforms). On change, the state patch flows to
  attached clients through the same mechanism as `isBashRunning`.
- Before the kernel has booted (it starts lazily), `kernelCwd` is unset and
  consumers fall back to the session cwd.

## 2. Footer

`FooterComponent`
(`packages/coding-agent/src/modes/interactive/components/footer.ts`, currently
intentionally empty) renders a single line:

- Path: `kernelCwd ?? session cwd`, home-abbreviated with `~`, truncated to
  terminal width.
- Branch: from the existing `FooterDataProvider`. When the displayed cwd
  changes, the existing `footerDataProvider.setCwd(...)` call re-roots branch
  resolution and git watching so path and branch always agree.
- File autocomplete (`CombinedAutocompleteProvider` base dir) also follows the
  displayed cwd.

## 3. `/term` command

Built-in slash command in interactive mode.

Busy check: reuse the UI's existing busy predicate (streaming / compacting /
bash running / active or queued session actions). If busy, print a hint and do
nothing.

When idle:

1. Suspend the TUI using the existing Ctrl+Z suspend/resume machinery (leave
   alt screen, restore terminal modes, keep-alive interval, SIGINT guard).
2. Spawn `$SHELL` (fallback `/bin/sh`) with inherited stdio, `cwd` = current
   kernel cwd (fallback session cwd).
3. While the shell runs, poll its process cwd every ~300 ms:
   - Linux: `readlink /proc/<pid>/cwd`
   - macOS: `lsof -a -p <pid> -d cwd` (parsed)
   Shell-agnostic: no rc-file or prompt-hook injection; works with bash, zsh,
   fish.
4. On shell exit, resume the TUI. If the last observed cwd differs from the
   current kernel cwd, propagate it (section 4) and refresh footer +
   autocomplete.

## 4. Propagation path

New connection operation `setKernelCwd(dir)` on the agent-connection
interface, implemented in both adapters:

- **In-process adapter:** directly on the session runtime.
- **Daemon adapter:** new daemon command behind a **negotiated server
  capability** (per daemon protocol rules). Update `DAEMON_SCHEMA_REVISION`,
  the command/event compatibility maps, and add new-client/old-daemon plus
  old-client/new-daemon tests. `DAEMON_PROTOCOL_VERSION` is not bumped: the
  feature is optional and capability-gated, startup does not depend on it.

Semantics of `setKernelCwd(dir)`:

1. Execute a hidden `os.chdir(dir)` in the kernel (queued like any execution;
   if the kernel is not yet booted, record the cwd so the kernel provisioner
   boots into it instead).
2. Refresh `kernelCwd` and patch connection state.
3. Append a short notice into the session context — "Working directory changed
   to `<dir>` by the user via /term" — so the agent is not silently teleported.

Degradation: against an old daemon lacking the capability, `/term` still opens
the shell but shows a warning that cwd propagation is unavailable.

## 5. Error handling

- Shell exits nonzero or is killed: resume normally; propagation still uses the
  last observed cwd if one was seen.
- Observed cwd missing/unreadable at exit: skip propagation, show a warning.
- No cwd-polling mechanism on the platform (no /proc, no usable lsof): shell
  works; warn once that cd propagation is unavailable on this platform.
- Kernel dead or restarting at propagation time: the recorded cwd is applied at
  next kernel boot via the provisioner cwd.
- `/term` invoked while busy: rejected with hint (see section 3).

## 6. Testing

- Unit: cwd-poll helper (Linux path against a real child process); footer
  rendering (abbreviation, branch, truncation); `/term` busy-gating.
- Suite (`packages/coding-agent/test/suite/` harness + faux provider, no real
  providers): `kernelCwd` state patch after an ipython execution;
  `setKernelCwd` end-to-end in in-process mode.
- Daemon compat tests for the new capability in both directions.
- The suspend/interactive-shell step is verified manually via the tmux
  workflow in AGENTS.md (interactive shells are not unit-testable).

## Out of scope

- Retargeting session storage / `SessionManager` cwd mid-session.
- Embedded terminal emulator or split view.
- Queueing propagation while the agent is mid-run (possible follow-up).
