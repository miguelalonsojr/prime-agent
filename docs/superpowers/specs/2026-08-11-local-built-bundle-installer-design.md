# Local Built-Bundle Installer Design

## Goal

Install the current checkout as the `prime-agent` command without using the TypeScript source runner. The command must execute the bundle produced by `npm run build` and work from any directory.

## Installer

Add `scripts/install-local.sh`. It is a POSIX shell script intended to run from a Prime Agent checkout.

The script accepts no arguments or the single `--force` argument. Any other argument, duplicate `--force`, or combined flag fails with a usage message.

The target is `$HOME/.local/bin/prime-agent`. The installer creates `$HOME/.local/bin` when needed. It fails if `HOME` is unset, if the target parent cannot be created, or if the target is a directory.

If the target exists, the installer exits without changing it unless `--force` was passed. A forced installation replaces only the target command; it does not remove other files.

The installer checks the target collision before building. It then runs `npm run build` from the checkout root. A failed build leaves any existing command unchanged.

After a successful build, it writes a temporary wrapper beside the destination and renames it into place. The wrapper has an absolute reference to the checkout and ends with:

```sh
exec /absolute/path/to/checkout/prime-agent.sh --dist "$@"
```

The wrapper starts a bundled Node.js CLI. It does not invoke `tsx` or compile TypeScript at startup. The wrapper has one shell `exec` before `prime-agent.sh` executes Node.

The installer's output identifies the installed path. When `$HOME/.local/bin` is absent from `PATH`, it also prints the command needed to add it for future shells:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

Moving or deleting the checkout invalidates the generated wrapper. Rerun the installer from its new location after moving the checkout. Run `npm run build` after source changes; rebuilding updates the bundle without reinstalling.

## Documentation and tests

Document the local built-bundle install command in the development and quickstart documentation. Add an unreleased coding-agent changelog entry.

Add focused tests for argument validation, normal wrapper creation, collision refusal, forced replacement, forwarding user arguments to the bundled launcher, and PATH guidance. Tests use a temporary home directory and replace the build command in `PATH` so they do not modify a real user command or require a full production build.

## Non-goals

This is not a replacement for the release installer. It does not install Node.js, package dependencies, or make the source workspace's inherited `pi` npm bin public. It does not create a global npm link.
