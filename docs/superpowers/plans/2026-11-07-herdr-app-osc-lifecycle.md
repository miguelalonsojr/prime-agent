# Herdr App OSC Lifecycle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An interactive Prime Agent running in a Herdr App terminal reports its root activity as OSC 7501 so the left Agents panel lists it and tracks turns.

**Architecture:** A small terminal-only, process-lifetime OSC reporter in `pa-tui` owns the last published state, retry grace, and queued-work debounce. The interactive session UI sends turn/snapshot boundaries to that reporter, while the CLI clears its status at process exit. This keeps the existing `pa-daemon` Herdr TUI socket integration untouched.

**Tech Stack:** Rust, crossterm/ratatui, Tokio, existing telemetry catalog, Herdr Core OSC 7501 parser.

**Spec:** `docs/superpowers/specs/2026-11-07-herdr-app-osc-lifecycle-design.md`

## Global Constraints

- Current branch is `feature/herdr-app-osc-20261107` in its own worktree. Do not edit CLM worktrees or main.
- OSC 7501 root wire uses `ESC ] 7501 ; state=<word>:app=prime-agent ESC \`. No `id` and no payload from the session.
- No terminal writes on daemon/worker/headless/non-Herdr processes. No network/disk synchronization in paint or event handling. Keep the separate `HERDR_*` socket reporter intact.
- Use an active-session snapshot on attach. Do not use frame timing to infer turn transitions. Do not let a stale event overwrite a newer root turn.
- Preserve the 2500 ms provider-error retry grace and 250 ms idle debounce only when queued work exists. Report `blocked` only for final provider errors; aborts settle idle.
- Add one privacy-safe, fire-and-forget adoption telemetry event; update the versioned schema without changing wire APIs or dependencies. Keep changes minimal. Add `.changes/herdr-app-osc-20261107.md`.
- Baseline suite: harness path overrides caused two ACP failures; removing both overrides fixes them. A separate differential test expects a TS reference binary but PATH resolves to Rust. Report that verifier limitation separately.

## Review Focus

- A new session opens while an earlier process remains busy: attach snapshot must report working; Task 2 tests it.
- An error-ended turn is followed by an automatic retry: do not publish blocked or idle between them; Task 1 tests it.
- A child agent performs work in the daemon without owning a terminal: never emit its own root report; Task 2 tests this.
- A user exits chat through the Agents view or switches sessions: do not retain the previous turn's working state; Task 2 tests handoff.
- The process exits into a shell or starts in a piped context: clear or suppress the status respectively; Tasks 1 and 2 test these.

---

### Task 1: Terminal reporter and direct OSC verification

**Files:** Create `crates/pa-tui/src/herdr_app.rs`; modify `crates/pa-tui/src/lib.rs`. Keep policy and writer in this module. Use an injected `Write` in tests so bytes reach the actual core parser fixture rather than only matching a string.

**Interfaces:** `HerdrAppStatus::new(enabled: bool)`, `session_open(active: bool)`, `turn_started()`, `turn_ended(error: bool, more_queued: bool)`, `retry_started()`, `settle(now: Instant)`, `idle_for_handoff()`, `clear()`. One process-lifetime facade emits to terminal stdout only after `HC_TERMINAL_ID` and `HC_TERMINAL_SOCKET` exist and stdout is a terminal; expose only what the CLI/run path must call. No additions to `pa-types`.

- [ ] Load the `clean-coding` skill, then write focused tests for root OSC 7501 exact bytes, dedup, start/settle/retry/error timing, explicit handoff, quit clear, and disabled/non-TTY silence.
- [ ] Run `cargo test -p pa-tui herdr_app::tests -- --nocapture` and observe the new tests fail before implementation.
- [ ] Implement a minimal reporter. Use ST termination and no `id`. Arrange deadlines in the interactive event loop (not a rendering callback or background stdout writer). Do not flush if no terminal.
- [ ] Re-run the focused tests and inspect `git diff` before extending to the UI.

### Task 2: Wire TUI lifecycle to a single process-scoped root report

**Files:** Modify `crates/pa-tui/src/interactive/run.rs` (session open/snapshot, live batched events, fallback/quit, handoffs), `crates/pa-tui/src/session_ui/apply.rs` as needed to retain terminal assistant error state, `crates/pa-cli/src/interactive_mode.rs` (process exit and handoff), and existing PTY test file `crates/pa-cli/tests/input_live_open_pty_e2e.rs` or another already-run PTY test as needed. Avoid growing `interactive/run.rs` with reporter policy.

**Interfaces:** Uses Task 1 `HerdrAppStatus` ownership and methods. Terminal UI receives the snapshot `turn_active` from `SessionUi::attach_session`; state changes come from events after `apply_client_event`. Existing `session_rlm_depth`/`SessionSelection::NewChild` prevents duplicate child reports within one PTY.

- [ ] Load `clean-coding` and `test-driven-development`; write one red-first PTY test that opens a faux session with `HC_TERMINAL_ID`/`HC_TERMINAL_SOCKET` and proves `hc`-compatible start (`idle`), turn (`working`), settle (`idle`), error-to-blocked, handoff and clear, plus terminal silence in headless. Do not assert on a formatted value without capturing actual PTY bytes.
- [ ] Run the targeted test to observe failure.
- [ ] Wire attach snapshot and each event transition to the reporter. Keep last assistant `stopReason=error` separate from generic `turn_end.error`, and respect queued followups and retry grace. Clear the root report once at the end of the entire CLI process, not on each chat-to-Agents-view switch.
- [ ] Re-run focused PTY and `pa-tui` tests. Exercise the built binary in a separate `hc` terminal, verify `hc get <new-terminal> --json` reports `status.app=prime-agent`, then clean up only that new terminal.

### Task 3: Privacy-safe adoption event, changelog, and full validation

**Files:** Modify `crates/pa-telemetry/src/catalog.rs` and `crates/pa-cli/src/interactive_mode.rs` (reuse existing startup telemetry client), add `.changes/herdr-app-osc-20261107.md`, update `crates/pa-tui/README.md` if its scope changes. No dependency change.

**Interfaces:** Event `herdr app integration enabled`, no content-bearing properties, tracked once per interactive terminal process when OSC reporting is active; use fire-and-forget existing telemetry worker.

- [ ] Load `clean-coding`, `plain-technical-prose`, and `test-driven-development`. Add a red-first test for the new schema event and its one-time conditional emit; verify no prompt/session/file content can enter properties.
- [ ] Add the event rule and track at the CLI startup seam without awaiting telemetry delivery. Add one shipped-path changelog bullet and update ownership docs only if their described scope changed.
- [ ] Run `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `env -u PRIME_AGENT_CODING_AGENT_DIR -u PRIME_AGENT_SESSION_DIR cargo test --workspace` from this worktree; if the differential test fails due to wrong TS binary, report it and run all other suites plus focused verification instead of masking it.
- [ ] Review diff for scope, constants, dependencies, and the CLI/GUI behavior. Report exact verifier outcomes and worktree path without merging into main or CLM work.
