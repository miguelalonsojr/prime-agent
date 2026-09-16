import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { lstatSync, mkdtempSync, mkdirSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
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
	assert.equal(readFileSync(collision.buildLog, "utf8"), "");
	assertCommand(runInHome(collision.home, ["--force"], collision), 0);

	const danglingLink = runInIsolatedHome([], { danglingCommand: "missing-target" });
	assert.notEqual(danglingLink.status, 0);
	assert.equal(readFileSync(danglingLink.buildLog, "utf8"), "");
	assert.equal(lstatSync(danglingLink.commandPath).isSymbolicLink(), true);
	const forcedDanglingLink = runInHome(danglingLink.home, ["--force"], danglingLink);
	assertCommand(forcedDanglingLink, 0);
	assert.equal(lstatSync(danglingLink.commandPath).isSymbolicLink(), false);

	assert.notEqual(runInIsolatedHome(["--unknown"]).status, 0);
	assert.match(runInIsolatedHome([]).stdout, /export PATH=/);
	const directory = runInIsolatedHome(["--force"], { destinationDirectory: true });
	assert.notEqual(directory.status, 0);
	assert.equal(readFileSync(directory.buildLog, "utf8"), "");

	const failedBuild = runInIsolatedHome(["--force"], {
		existingCommand: "stable launcher\n",
		buildStatus: 17,
	});
	assert.equal(failedBuild.status, 17);
	assert.equal(readFileSync(failedBuild.commandPath, "utf8"), "stable launcher\n");
	assert.match(readFileSync(failedBuild.buildLog, "utf8"), /run build/);

	runInApostropheCheckout();
	console.log("Local installer check passed.");
} finally {
	for (const directory of temporaryDirectories) {
		rmSync(directory, { recursive: true, force: true });
	}
}

function runInIsolatedHome(arguments_, options = {}) {
	const tempDir = trackTemporaryDirectory();
	const home = join(tempDir, "home");
	const bin = join(tempDir, "bin");
	const buildLog = join(tempDir, "build.log");
	const commandPath = join(home, ".local", "bin", "prime-agent");
	mkdirSync(home);
	mkdirSync(bin);
	writeFileSync(buildLog, "", "utf8");
	writeFakeNpm(bin);

	if (options.existingCommand !== undefined) {
		mkdirSync(dirname(commandPath), { recursive: true });
		writeFileSync(commandPath, options.existingCommand, "utf8");
	} else if (options.danglingCommand !== undefined) {
		mkdirSync(dirname(commandPath), { recursive: true });
		symlinkSync(options.danglingCommand, commandPath);
	} else if (options.destinationDirectory) {
		mkdirSync(commandPath, { recursive: true });
	}

	return runInHome(home, arguments_, { bin, buildLog, commandPath, buildStatus: options.buildStatus });
}

function runInHome(home, arguments_, paths = {}) {
	const bin = paths.bin ?? join(dirname(home), "bin");
	const buildLog = paths.buildLog ?? join(dirname(home), "build.log");
	const commandPath = paths.commandPath ?? join(home, ".local", "bin", "prime-agent");
	const result = spawnSync(installerPath, arguments_, {
		cwd: root,
		encoding: "utf8",
		env: {
			...process.env,
			HOME: home,
			PATH: `${bin}:${process.env.PATH ?? ""}`,
			BUILD_LOG: buildLog,
			FAKE_NPM_STATUS: String(paths.buildStatus ?? 0),
		},
	});
	return { ...result, home, bin, buildLog, commandPath, buildStatus: paths.buildStatus };
}

function runInApostropheCheckout() {
	const tempDir = trackTemporaryDirectory();
	const checkout = join(tempDir, "checkout'quoted");
	const scripts = join(checkout, "scripts");
	const home = join(tempDir, "home");
	const bin = join(tempDir, "bin");
	const buildLog = join(tempDir, "build.log");
	const commandPath = join(home, ".local", "bin", "prime-agent");
	mkdirSync(scripts, { recursive: true });
	mkdirSync(home);
	mkdirSync(bin);
	writeFileSync(buildLog, "", "utf8");
	writeFileSync(join(scripts, "install-local.sh"), readFileSync(installerPath, "utf8"), "utf8");
	writeFileSync(
		join(checkout, "prime-agent.sh"),
		'#!/bin/sh\nfor argument in "$@"; do printf "<%s>\\n" "$argument"; done\n',
		{ encoding: "utf8", mode: 0o755 },
	);
	writeFakeNpm(bin);
	const environment = {
		...process.env,
		HOME: home,
		PATH: `${bin}:${process.env.PATH ?? ""}`,
		BUILD_LOG: buildLog,
		FAKE_NPM_STATUS: "0",
	};
	assertCommand(spawnSync("sh", [join(scripts, "install-local.sh")], { cwd: checkout, encoding: "utf8", env: environment }), 0);
	assertCommand(spawnSync("sh", ["-n", commandPath], { encoding: "utf8", env: environment }), 0);
	const launcher = spawnSync(commandPath, ["first", "space value"], { encoding: "utf8", env: environment });
	assertCommand(launcher, 0);
	assert.equal(launcher.stdout, "<--dist>\n<first>\n<space value>\n");
}

function assertCommand(result, status) {
	assert.equal(result.error, undefined, result.error?.message);
	assert.equal(result.status, status, `stdout: ${result.stdout}\nstderr: ${result.stderr}`);
}

function trackTemporaryDirectory() {
	const directory = mkdtempSync(join(tmpdir(), "prime-agent-local-installer-"));
	temporaryDirectories.push(directory);
	return directory;
}

function writeFakeNpm(bin) {
	writeFileSync(
		join(bin, "npm"),
		'#!/bin/sh\nprintf "%s\\n" "$*" >> "$BUILD_LOG"\nexit "${FAKE_NPM_STATUS:-0}"\n',
		{ encoding: "utf8", mode: 0o755 },
	);
}
