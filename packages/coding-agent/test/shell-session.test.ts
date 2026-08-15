import { mkdtempSync, realpathSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { cwdPollingSupported, readProcessCwd, runShellSession } from "../src/modes/interactive/shell-session.js";

const itPosix = process.platform === "linux" || process.platform === "darwin" ? it : it.skip;
const itLinux = process.platform === "linux" ? it : it.skip;

describe("readProcessCwd", () => {
	itPosix("reads this process's cwd", () => {
		expect(readProcessCwd(process.pid)).toBe(process.cwd());
	});

	itPosix("returns null for a dead pid", () => {
		expect(readProcessCwd(999999999)).toBeNull();
	});
});

describe("runShellSession", () => {
	itLinux("observes a cd performed inside the shell", async () => {
		const dir = realpathSync(mkdtempSync(join(tmpdir(), "shell-session-")));
		try {
			const result = await runShellSession({
				shell: "/bin/sh",
				shellArgs: ["-c", `cd "${dir}" && sleep 0.5`],
				cwd: tmpdir(),
				pollIntervalMs: 25,
			});
			expect(result.exitCode).toBe(0);
			expect(result.lastObservedCwd).toBe(dir);
		} finally {
			rmSync(dir, { recursive: true, force: true });
		}
	});

	itPosix("rejects when the shell binary does not exist", async () => {
		await expect(runShellSession({ shell: "/no/such/shell", cwd: tmpdir() })).rejects.toThrow();
	});
});

describe("cwdPollingSupported", () => {
	it("matches the platform", () => {
		expect(cwdPollingSupported()).toBe(process.platform === "linux" || process.platform === "darwin");
	});
});
