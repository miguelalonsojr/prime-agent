import { mkdtempSync, realpathSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { cwdPollingSupported, readProcessCwd, runShellSession } from "../src/modes/interactive/shell-session.js";

describe("readProcessCwd", () => {
	it("reads this process's cwd on supported platforms", () => {
		const cwd = readProcessCwd(process.pid);
		if (cwdPollingSupported()) expect(realpathSync(cwd!)).toBe(realpathSync(process.cwd()));
		else expect(cwd).toBeNull();
	});

	it("returns null for a dead pid", () => {
		expect(readProcessCwd(999999999)).toBeNull();
	});
});

describe("runShellSession", () => {
	it("captures the final cwd when a shell changes directory and exits immediately", async () => {
		const dir = realpathSync(mkdtempSync(join(tmpdir(), "shell-session-")));
		try {
			const result = await runShellSession({
				shell: "/bin/sh",
				shellArgs: ["-c", `cd ${JSON.stringify(dir)}; exit 23`],
				cwd: tmpdir(),
			});
			expect(result.exitCode).toBe(23);
			expect(result.lastObservedCwd).toBe(dir);
		} finally {
			rmSync(dir, { recursive: true, force: true });
		}
	});

	it("rejects when the shell binary does not exist", async () => {
		await expect(runShellSession({ shell: "/no/such/shell", cwd: tmpdir() })).rejects.toThrow();
	});
});

describe("cwdPollingSupported", () => {
	it("supports only Linux and macOS", () => {
		expect(cwdPollingSupported("linux")).toBe(true);
		expect(cwdPollingSupported("darwin")).toBe(true);
		expect(cwdPollingSupported("win32")).toBe(false);
	});
});
