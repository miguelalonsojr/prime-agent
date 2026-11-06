# Rust Main Handoff Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make fork `main` an upstream Rust branch with a local checkout installer, then stop before replaying the two archived feature commits.

**Architecture:** A separate worktree starts at the verified upstream tip. One installer-phase commit contains the checkout installer, its focused test, and the approved handoff documents. An exact-tip force-with-lease updates only fork `main` after test and session-copy gates pass.

**Tech Stack:** Git, POSIX shell, Rust workspace (CI Rust 1.98.1), isolated temporary Python runtime.

**Spec:** `docs/superpowers/specs/2026-11-06-rust-main-handoff-design.md`

## Global Constraints

- Preserve `origin/archive/rust-port-c466d89b` and both archived old `main` tips. Never push to upstream.
- The upstream base is `74591cddef28d11249c3171112114fc4755375d7`; recheck its live ref before staging. If it moved, stop and update the design.
- Do not touch the active Node daemon, original session JSONL, shared personal kernel venv, or current installed Rust launcher during staging.
- Keep all verification processes under a temporary `HOME`, `TMPDIR`, agent directory, socket, and kernel venv. Use `env -i` and a verified `PA_TS_BINARY` for TypeScript differential tests; the installed `prime-agent` on PATH is Rust.
- No unrelated upstream repairs. A failed verification, copy-resume trial, or lease check blocks promotion.
- User approved a non-fast-forward push. Use only `--force-with-lease=refs/heads/main:<observed-sha>` after a fresh remote-ref check; never plain `--force`.
- Stage one ends when `main` has the installer and the user has the exact install and safe resume commands. Feature and CLM replay are not part of this plan.

## Review Focus

- An existing launcher survives a failed local build; the installer does not publish a broken command.
- An explicit `--force` replaces a launcher without deleting a symlink target or a directory.
- The launcher executes the binary from the stable `main` worktree, forwards quoted arguments, and remains valid after the agent session ends.
- A CLM-off Node session copy opens under upstream-only Rust without changing the original file or reaching a paid provider.
- A moving `origin/main` or `upstream/main` prevents an unsafe push; archived tips remain reachable.

---

### Task 1: Stage a pinned upstream worktree and qualify the test reference

**Files:** `docs/superpowers/specs/2026-11-06-rust-main-handoff-design.md`, `docs/superpowers/plans/2026-11-06-rust-main-handoff.md` (copy these approved documents into the staging worktree).

- [ ] Use `using-git-worktrees` to create a clean, ignored staging worktree at the exact fetched upstream SHA. Confirm `git rev-parse HEAD` and `git status --short` before edits.
- [ ] Recheck `git ls-remote upstream refs/heads/main` and all three remote archive refs. Stop if a tip or archive differs from the design.
- [ ] Obtain the official v0.9.8 TypeScript release binary named by upstream `.github/workflows/ci.yml`, verify its documented SHA-256 before extraction, and set `PA_TS_BINARY` to its absolute executable path. Prove `--version` and that it is not the installed Rust launcher.
- [ ] On the pristine upstream tip, run the CLI differential case with explicit `PA_TS_BINARY` and compare its mismatch set to the recorded port result. Run `make check` separately in the upstream CI-equivalent environment (no TypeScript executable on PATH in shard 5), recording the conditional corpus early return and the complete gate result. Stop and classify any new failure before code; never report the conditional corpus as verified parity.

### Task 2: Add the checkout installer with a focused regression

**Files:** Create `scripts/install-local.sh` from archived customization commit `7d3c8d64`; create `crates/pa-cli/tests/local_installer_e2e.rs` and `.changes/rust-main-local-installer.md`; copy the approved design and plan from this checkout.

- [ ] Load the `clean-coding` skill. Add a small behavior-focused test for failed-build preservation and successful `--force` publication/argument forwarding. Keep test additions within the repository's test/source line limit.
- [ ] Run `cargo test -p pa-cli --test local_installer_e2e` from the Rust worktree and record a failing result while the script is absent.
- [ ] Copy the previously tested installer verbatim from commit `7d3c8d64`. Change it only if the current upstream build interface requires it. Add one user-facing changelog fragment required by the Rust branch. Do not copy unrelated customization files.
- [ ] Run the modified test directly and confirm it passes. Temporarily break the behavior in a disposable test copy to confirm the test detects the regression, then restore the copy. Inspect `git diff --check` and file-scoped diff.
- [ ] Run `scripts/install-local.sh --force` with temporary `HOME` and isolated Cargo/kernel paths. Verify the installed temp launcher executes the staged Rust binary and that the real `~/.local/bin/prime-agent` remains unchanged.

### Task 3: Verify the upstream-only session handoff and full gates

**Files:** No product source beyond Task 2; evidence remains under a dedicated temporary directory.

- [ ] When the current session file has a complete JSONL tail, copy it into isolated state and validate every row. Hash the original before and after the trial. Never open the live source file from a second daemon.
- [ ] Start the staged Rust binary on a unique socket with temporary `HOME`, agent dir, session dir, and kernel venv. Resume the copy, inspect visible session context, and stop only owned processes. No real provider call or credentials. Stop promotion if this session cannot resume safely.
- [ ] Run `make check` on the staged tree with CI Rust 1.98.1 and the CI-equivalent environment. Record fmt, Clippy, workspace tests, release build, ignored tests, and complete output. Separately run the explicit-reference CLI differential case and compare with the pristine upstream mismatch set; stop on new differences. Do not label the conditional corpus early return as parity coverage.
- [ ] Review installer file ownership, dependency direction, source diff, and manual evidence. Resolve only installer-scope findings before promotion.

### Task 4: Commit, promote, verify, and hand off

**Files:** Commit only the installer, focused test, changelog fragment, approved spec, and plan from Task 2. Do not commit temporary trial artifacts.

- [ ] Verify staging status and commit those exact files with one installer-phase commit. Keep archived `7d3c8d64` and `c466d89b` unchanged.
- [ ] Confirm `origin/main` still equals the backed-up old tip. Move the unoccupied local `main` ref to the verified staging commit, then push that SHA to `origin/main` with an exact `--force-with-lease`. If the remote moved or protection rejects the push, stop without another push.
- [ ] Verify local and remote `main` SHAs, first-parent upstream ancestry, the installer commit, and archive refs. Create and retain a stable local `main` worktree for the source installer.
- [ ] Give the user the exact `scripts/install-local.sh --force` path and a safe separate-socket/venv resume command. State that the Node writer must stop before the original session file is opened by Rust. Stop before applying the two archived feature commits.
