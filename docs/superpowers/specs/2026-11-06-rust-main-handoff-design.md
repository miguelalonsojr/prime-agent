# Rust main handoff design

## Intent

Move the fork's `main` to current upstream Rust and add a local-source installer before replaying the two feature commits. Stop after the installer handoff so the user can install and resume the current session. Preserve both old fork histories and the original Rust-port commits.

## Current state

- Upstream `main` is `74591cddef28d11249c3171112114fc4755375d7`; the Rust port `c466d89b` contains customization `7d3c8d64` and native CLM `c466d89b` above v0.10.2. The fork's remote `main` is `c4e4f6d4`; local `main` now points to upstream Rust, with its former `45b245c9` tip archived.
- Remote archive branches already preserve the Rust port and both old `main` tips. No archive branch is removed during migration.
- Upstream has `install-rust.sh`, which installs a published release and may stop daemons. It lacks the checkout-bound `scripts/install-local.sh` required for this handoff.
- The current `prime-agent` launcher points to the earlier Rust port worktree. The current session still runs under Node. A prior test populated the shared kernel venv with Rust files; no live-home repair is part of this work.

## Approaches

- Selected: stage a branch from the live upstream Rust tip, add one installer commit, verify it, then replace `origin/main` using `--force-with-lease` against its exact old tip. This produces an upstream-aligned `main` with a local installer and preserves the two feature commits for later replay.
- Rejected: merge upstream into the TypeScript fork `main`. It retains the old ancestry but cannot produce the requested linear Rust base and carries unrelated TypeScript conflicts.
- Rejected: install from the existing Rust-port worktree and move `main` only after both feature commits. It avoids the intermediate commit but does not provide `install-local.sh` on upstream-aligned `main` before the user resumes.

## First handoff: installer and main

1. Confirm the remote archive refs for `c466d89b`, `c4e4f6d4`, and `45b245c9`. Recheck the live upstream and fork `main` SHAs before staging or pushing. Stop if either moved.
2. Use an isolated worktree based on `74591cdd`. Add `scripts/install-local.sh` from the preserved Rust customization commit and a focused installer test. Keep the installer behavior limited to local release build and atomic launcher publication; no daemon shutdown or live-session migration.
3. Run the Rust checks with CI's pinned Rust 1.98.1. Exercise the installer and installed binary with a temporary `HOME`, settings, session directory, daemon socket, and kernel venv. Confirm that a copy of the current CLM-off Node session opens on the upstream-only Rust binary without modifying the source session. If the copy cannot open safely, stop before changing `main`.
4. If verification passes, commit the installer, its test, and the approved handoff design/plan as one installer-phase commit. Recheck `origin/main`. Advance local `main` from upstream Rust to the staged commit while retaining both archive tips, then push only `main` using `--force-with-lease=refs/heads/main:<verified-old-sha>`. Verify the remote and local refs and leave an accessible local `main` worktree for installation. Never use a plain force push.
5. Give the user the exact `scripts/install-local.sh --force` path in the stable `main` worktree and an isolated daemon/venv resume command. The user must stop the Node writer before resuming the original session file. The installed launcher does not switch this already-running Node process. Stop work at this handoff.

## Later handoff: feature commits

After the user resumes, replay the customization and CLM functionality from `archive/rust-port-c466d89b` as two logical commits on the new upstream-plus-installer base. Preserve upstream's new kernel timeout, RLM, billing, and shutdown behavior while integrating CLM. Run focused and full checks before pushing the two commits. Their new commit IDs will differ from the originals. This later work does not start before the first handoff.

## Safety and limits

- Do not edit or delete the live session, the shared kernel venv, the existing Node daemon, or the archive branches. Use temporary isolated state for all compatibility trials.
- A failed upstream baseline, failed session-copy trial, failed installer smoke, test regression, moving remote ref, or denied force-with-lease stops the promotion. Do not fix unrelated upstream failures as part of the handoff.
- The archive refs permit a later, separately approved rollback; do not rewrite `main` a second time while a session writes to it.
