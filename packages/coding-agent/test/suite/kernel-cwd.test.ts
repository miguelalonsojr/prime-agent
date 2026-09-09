import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AgentSession } from "../../src/core/agent-session.js";
import { TERM_CWD_CHANGED_CUSTOM_TYPE } from "../../src/core/messages.js";
import type { IpythonKernelProvisioner } from "../../src/core/tools/ipython.js";
import { createHarness, type Harness } from "./harness.js";

type KernelCwdHost = {
	_refreshKernelCwd(): Promise<void>;
	_ipythonKernelProvisioner?: IpythonKernelProvisioner;
};

function replaceProvisioner(session: AgentSession, provisioner: Partial<IpythonKernelProvisioner>): void {
	Reflect.set(session, "_ipythonKernelProvisioner", {
		dispose: async () => {},
		setCwd: () => {},
		chdir: async () => null,
		readCwd: async () => null,
		...provisioner,
	});
}

describe("kernel cwd session boundary", () => {
	const harnesses: Harness[] = [];
	afterEach(() => {
		while (harnesses.length) harnesses.pop()?.cleanup();
	});

	it("refreshes only changed successful running-kernel cwd values", async () => {
		const harness = await createHarness();
		harnesses.push(harness);
		const readCwd = vi.fn(async () => join(harness.tempDir, "changed"));
		replaceProvisioner(harness.session, { hasRunningKernel: true, readCwd });
		const host = harness.session as unknown as KernelCwdHost;
		await host._refreshKernelCwd();
		await host._refreshKernelCwd();
		expect(harness.session.kernelCwd).toBe(join(harness.tempDir, "changed"));
		expect(harness.eventsOfType("kernel_cwd_changed")).toEqual([
			{ type: "kernel_cwd_changed", cwd: join(harness.tempDir, "changed") },
		]);
	});

	it("silently ignores absent, failed, and timed out probes", async () => {
		const harness = await createHarness();
		harnesses.push(harness);
		const host = harness.session as unknown as KernelCwdHost;
		replaceProvisioner(harness.session, { hasRunningKernel: false });
		await host._refreshKernelCwd();
		replaceProvisioner(harness.session, {
			hasRunningKernel: true,
			readCwd: async () => {
				throw new Error("nope");
			},
		});
		await host._refreshKernelCwd();
		vi.useFakeTimers();
		try {
			replaceProvisioner(harness.session, {
				hasRunningKernel: true,
				readCwd: async (signal) =>
					new Promise((_, reject) =>
						signal?.addEventListener("abort", () => reject(new Error("aborted")), { once: true }),
					),
			});
			const refresh = host._refreshKernelCwd();
			await vi.advanceTimersByTimeAsync(5_000);
			await refresh;
		} finally {
			vi.useRealTimers();
		}
		expect(harness.eventsOfType("kernel_cwd_changed")).toEqual([]);
	});

	it("sets a validated preboot cwd and queues a hidden next-turn notice", async () => {
		const harness = await createHarness();
		harnesses.push(harness);
		const nextDir = mkdtempSync(join(tmpdir(), "prime-agent-session-cwd-"));
		const setCwd = vi.fn();
		try {
			replaceProvisioner(harness.session, { hasRunningKernel: false, setCwd });
			await harness.session.setKernelCwd(nextDir);
			expect(setCwd).toHaveBeenCalledWith(nextDir);
			expect(harness.session.kernelCwd).toBe(nextDir);
			expect(harness.eventsOfType("kernel_cwd_changed")).toEqual([{ type: "kernel_cwd_changed", cwd: nextDir }]);
			expect(harness.session.getPendingNextTurnMessageSnapshots()).toContainEqual(
				expect.objectContaining({
					customType: TERM_CWD_CHANGED_CUSTOM_TYPE,
					display: false,
					details: { cwd: nextDir },
				}),
			);
			const file = join(nextDir, "file");
			writeFileSync(file, "x");
			const eventCount = harness.eventsOfType("kernel_cwd_changed").length;
			const noticeCount = harness.session.getPendingNextTurnMessageSnapshots().length;
			await expect(harness.session.setKernelCwd(join(nextDir, "missing"))).rejects.toThrow();
			await expect(harness.session.setKernelCwd(file)).rejects.toThrow();
			expect(harness.session.kernelCwd).toBe(nextDir);
			expect(harness.eventsOfType("kernel_cwd_changed")).toHaveLength(eventCount);
			expect(harness.session.getPendingNextTurnMessageSnapshots()).toHaveLength(noticeCount);
		} finally {
			rmSync(nextDir, { recursive: true, force: true });
		}
	});

	it("changes a live kernel before publishing its cwd", async () => {
		const harness = await createHarness();
		harnesses.push(harness);
		const nextDir = mkdtempSync(join(tmpdir(), "prime-agent-session-cwd-"));
		const chdir = vi.fn(async (dir: string) => dir);
		const setCwd = vi.fn();
		try {
			replaceProvisioner(harness.session, { hasRunningKernel: true, chdir, setCwd });
			await harness.session.setKernelCwd(nextDir);
			expect(chdir).toHaveBeenCalledBefore(setCwd);
			replaceProvisioner(harness.session, {
				hasRunningKernel: true,
				chdir: async () => {
					throw new Error("failed");
				},
				setCwd,
			});
			const eventCount = harness.eventsOfType("kernel_cwd_changed").length;
			const noticeCount = harness.session.getPendingNextTurnMessageSnapshots().length;
			await expect(harness.session.setKernelCwd(nextDir)).rejects.toThrow("failed");
			expect(harness.session.kernelCwd).toBe(nextDir);
			expect(harness.eventsOfType("kernel_cwd_changed")).toHaveLength(eventCount);
			expect(harness.session.getPendingNextTurnMessageSnapshots()).toHaveLength(noticeCount);
		} finally {
			rmSync(nextDir, { recursive: true, force: true });
		}
	});
});
