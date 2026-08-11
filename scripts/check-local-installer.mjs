import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const installerPath = join(root, "scripts", "install-local.sh");
const temporaryDirectories = [];

try {
	const normal = runInIsolatedHome([]);
	assertCommand(normal, 0);
	assert.match(readFileSync(normal.commandPath, "utf8"), /prime-agent\.sh' --dist "\$@"/);
	assert.match(readFileSync(normal.buildLog, "utf8"), /run build/);

	const collision = runInIsolatedHome([], { existingCommand: "old command\n" });
	assert.notEqual(collision.status, 0);
	assert.equal(readFileSync(collision.commandPath, "utf8"), "old command\n");
	assertCommand(runInHome(collision.home, ["--force"]), 0);

	assert.notEqual(runInIsolatedHome(["--unknown"]).status, 0);
	assert.match(runInIsolatedHome([]).stdout, /export PATH=/);

	const directory = runInIsolatedHome(["--force"], { destinationDirectory: true });
	assert.notEqual(directory.status, 0);
	assert.equal(readFileSync(directory.buildLog, "utf8"), "");

	const apostrophePath = runInApostropheCheckout();
	assertCommand(apostrophePath.install, 0);
	assertCommand(spawnSync("sh", ["-n", apostrophePath.commandPath], { encoding: "utf8" }), 0);
	const forwarded = spawnSync("sh", [apostrophePath.commandPath, "first", "space value"], { encoding: "utf8" });
	assertCommand(forwarded, 0);
	assert.equal(forwarded.stdout, "<--dist>\n<first>\n<space value>\n");

	console.log("Local installer check passed.");
} finally {
	for (const directory of temporaryDirectories) {
		rmSync(directory, { recursive: true, force: true });
	}
}

function runInIsolatedHome(arguments_, options = {}) {
	const home = mkdtempSync(join(tmpdir(), "prime-agent-local-installer-"));
	temporaryDirectories.push(home);
	const commandPath = join(home, ".local", "bin", "prime-agent");
	const buildLog = join(home, "build.log");
	const binDirectory = join(home, "bin");
	writeFileSync(buildLog, "");
	mkdirSync(binDirectory);
	writeFileSync(join(binDirectory, "npm"), '#!/bin/sh\nprintf "%s\\n" "$*" >> "$BUILD_LOG"\n', { mode: 0o755 });

	if (options.existingCommand !== undefined) {
		mkdirSync(dirname(commandPath), { recursive: true });
		writeFileSync(commandPath, options.existingCommand);
	}
	if (options.destinationDirectory) {
		mkdirSync(commandPath, { recursive: true });
	}

	return runInHome(home, arguments_, { buildLog, commandPath, binDirectory, installerPath: options.installerPath });
}

function runInApostropheCheckout() {
	const checkoutParent = mkdtempSync(join(tmpdir(), "prime-agent-local-checkout-"));
	temporaryDirectories.push(checkoutParent);
	const checkout = join(checkoutParent, "checkout'quoted");
	const checkoutInstaller = join(checkout, "scripts", "install-local.sh");
	mkdirSync(dirname(checkoutInstaller), { recursive: true });
	writeFileSync(checkoutInstaller, readFileSync(installerPath, "utf8"));
	writeFileSync(join(checkout, "prime-agent.sh"), '#!/bin/sh\nprintf "<%s>\\n" "$@"\n', { mode: 0o755 });

	const install = runInIsolatedHome([], { installerPath: checkoutInstaller });
	return { commandPath: install.commandPath, install };
}

function runInHome(home, arguments_, paths = {}) {
	const commandPath = paths.commandPath ?? join(home, ".local", "bin", "prime-agent");
	const buildLog = paths.buildLog ?? join(home, "build.log");
	const binDirectory = paths.binDirectory ?? join(home, "bin");
	const result = spawnSync("sh", [paths.installerPath ?? installerPath, ...arguments_], {
		cwd: root,
		env: {
			BUILD_LOG: buildLog,
			HOME: home,
			PATH: `${binDirectory}:${process.env.PATH}`,
		},
		encoding: "utf8",
	});

	return { ...result, buildLog, commandPath, home };
}

function assertCommand(result, status) {
	assert.equal(result.status, status, `stderr: ${result.stderr}\nstdout: ${result.stdout}`);
	assert.equal(typeof result.stdout, "string");
	assert.equal(typeof result.stderr, "string");
}
