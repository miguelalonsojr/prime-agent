# Herdr App OSC 7501 lifecycle reporting

## Intent

A Prime Agent interactive process launched in a Herdr App terminal appears in the left Agents panel and shows its current activity. The live Herdr Core record for the current terminal identifies the foreground executable as `prime-agent`, but has `reported=false` and `status=null`; the app includes a pane in its Agents list only after its root terminal has a status. The feature belongs in the Prime Agent foreground terminal output. It does not change Herdr App or Herdr Core.

## Goals and limits

- Emit a root OSC 7501 report with `app=prime-agent` when an interactive Prime Agent process owns a Herdr Core terminal. Report `idle` initially, `working` for an active root turn including retries, `blocked` after a provider failure settles without retry, and `idle` on a normal settle or chat-to-Agents-view handoff. Clear the root report when the interactive process ends so an enclosing shell does not remain in the Agents panel.
- Emit to the actual PTY stdout, outside ratatui cell content. Use the Core-supported ST terminator and no `id`: `\x1b]7501;state=working:app=prime-agent\x1b\\` (actual ESC bytes on the wire). State transitions reach Core even if several daemon events arrive between two UI frames. Deduplicate unchanged reports, but do not suppress a new `working` after `idle`.
- Gate output on a nonempty Herdr Core terminal identity (`HC_TERMINAL_ID`, `HC_TERMINAL_SOCKET`) and interactive terminal stdout. No OSC in headless, daemon, worker, redirected/piped output, or RLM sessions running only inside the daemon. Keep the existing `HERDR_*` socket reporter intact for the separate Herdr TUI.
- Use the already-received active-session snapshot to report an attached turn as `working`, not an invented `idle`. Ignore stale events from another session. While Prime Agent's own Agents view is foreground, report `idle`; subagents in the same process do not each claim a root report. A child run in a separate Herdr Core terminal reports only for that terminal.
- Best-effort terminal delivery must not block first paint on network or disk. No new dependencies or public wire/schema changes. Add an adoption telemetry event with no prompt, session, or file content; it is fire-and-forget when reporting is enabled.

## Approaches

1. Emit OSC 7501 from the foreground Prime Agent process (selected). A small process-lifetime reporter is owned by the interactive path; the session UI sends it root activity transitions. This uses Herdr Core's existing parser and Herdr App's existing `prime-agent` icon.
2. Extend the daemon's current `HERDR_*` socket reporter. Its socket protocol targets the Herdr TUI and the daemon does not write to the foreground Herdr App PTY, so it cannot make this app's panel show the session.
3. Add process-name heuristics to Herdr App. It would duplicate terminal status detection and report no reliable turn lifecycle; the app already accepts the necessary OSC protocol.

## Behavior

The interactive entry point starts one process-lifetime reporter before the first interactive frame and releases it after all chat/Agents-view handoffs. Terminal stdout writes use the exact Core wire format, for example `\x1b]7501;state=idle:app=prime-agent\x1b\\`. Keep state in the reporter rather than coupling it to ratatui rendering. The session open uses the attached snapshot's streaming/compacting state; subsequent active-session start, settle, and retry events update it as events arrive. Hold a terminal assistant provider failure in `working` for the existing Herdr retry grace (2500 ms), cancel the hold if another turn/retry starts, and report `blocked` only when no retry follows. A queued follow-up should not create an idle-to-working flicker; use the existing 250 ms idle debounce. Clear error state when a new turn begins. An abort is not a provider block. If the UI switches to Prime Agent's Agents view or the connection closes, it reports `idle` rather than claiming that an unobserved session is still working. The process-lifetime exit reports `clear`, even if chat failed to mount.

This is a terminal integration, not a remote daemon service: the reporter must never make stdout-writable state a prerequisite for daemon processing, frame paint, or the existing Herdr TUI connector. It does not claim per-subagent cards for children that do not own a separate Core terminal. It does not add message/title fields to OSC, because the panel only needs the state and app name. The app and Core are external primary references: `herdr-app/defaults/AGENTS.md` and `src/window.rs`; `herdr-core/crates/herdr-core/src/status.rs` and `crates/ghostty-vt/src/lib.rs`.

## Verification

Write a red-first focused test over the actual terminal-output boundary: exact OSC bytes and root fields, entry/snapshot, start-to-settle, retry success versus exhausted failure, abort, handoff, exit, and nonterminal/headless silence. Feed the captured bytes through Herdr Core's actual parser or check Core's `hc get ... --json` from an isolated test terminal, rather than trusting a formatter-only assertion. After the focused tests, run `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` with the harness's `PRIME_AGENT_CODING_AGENT_DIR` and `PRIME_AGENT_SESSION_DIR` unset. The differential daemon test requires a genuine TS reference binary via `PA_TS_BINARY` or must be reported as unverified; the local PATH wrapper points to Rust and is not that reference. Manually launch the rebuilt Prime Agent inside a Herdr App terminal and confirm Core changes `reported` to true and `status.app` to `prime-agent` with the expected state. The running process in the current user terminal is an older binary until explicitly rebuilt/relaunched.
