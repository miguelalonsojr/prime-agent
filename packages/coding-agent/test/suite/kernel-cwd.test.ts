import { mkdtempSync, realpathSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { TERM_CWD_CHANGED_CUSTOM_TYPE } from "../../src/core/messages.js";
import { createHarness } from "./harness.js";

type KernelCwdHost = {
	_ipythonKernelProvisioner?: {
		hasRunningKernel: boolean;
		readCwd(signal?: AbortSignal): Promise<string | null>;
	};
	_refreshKernelCwd(): Promise<void>;
};

describe("kernel cwd tracking", () => {
	it("emits kernel_cwd_changed when the probed cwd changes, and only then", async () => {
		const harness = await createHarness();
		try {
			const host = harness.session as unknown as KernelCwdHost;
			let probed = "/tmp/kernel-cwd-one";
			host._ipythonKernelProvisioner = {
				hasRunningKernel: true,
				readCwd: async () => probed,
			};

			await host._refreshKernelCwd();
			await host._refreshKernelCwd(); // unchanged: no second event
			probed = "/tmp/kernel-cwd-two";
			await host._refreshKernelCwd();

			const events = harness.eventsOfType("kernel_cwd_changed");
			expect(events.map((event) => event.cwd)).toEqual(["/tmp/kernel-cwd-one", "/tmp/kernel-cwd-two"]);
			expect(harness.session.kernelCwd).toBe("/tmp/kernel-cwd-two");
		} finally {
			harness.cleanup();
		}
	});

	it("does nothing when no kernel is running", async () => {
		const harness = await createHarness();
		try {
			const host = harness.session as unknown as KernelCwdHost;
			host._ipythonKernelProvisioner = {
				hasRunningKernel: false,
				readCwd: async () => "/never",
			};

			await host._refreshKernelCwd();

			expect(harness.eventsOfType("kernel_cwd_changed")).toHaveLength(0);
			expect(harness.session.kernelCwd).toBeUndefined();
		} finally {
			harness.cleanup();
		}
	});

	it("setKernelCwd before kernel boot records the cwd, emits the event, and leaves a context notice", async () => {
		const harness = await createHarness();
		const dir = realpathSync(mkdtempSync(join(tmpdir(), "term-cwd-")));
		try {
			await harness.session.setKernelCwd(dir);

			const events = harness.eventsOfType("kernel_cwd_changed");
			expect(events.map((event) => event.cwd)).toEqual([dir]);
			expect(harness.session.kernelCwd).toBe(dir);

			// Delivered as context before the next turn, so it lives in the pending
			// next-turn queue rather than in the message history.
			const notice = harness.session
				.getPendingNextTurnMessageSnapshots()
				.find((message) => message.customType === TERM_CWD_CHANGED_CUSTOM_TYPE);
			expect(notice).toBeDefined();
			expect(notice?.display).toBe(false);
			expect(String(notice?.content)).toContain(dir);
		} finally {
			rmSync(dir, { recursive: true, force: true });
			harness.cleanup();
		}
	});

	it("setKernelCwd rejects a missing directory", async () => {
		const harness = await createHarness();
		try {
			await expect(harness.session.setKernelCwd("/definitely/not/a/real/dir")).rejects.toThrow();
			expect(harness.eventsOfType("kernel_cwd_changed")).toHaveLength(0);
			expect(harness.session.getPendingNextTurnMessageSnapshots()).toHaveLength(0);
		} finally {
			harness.cleanup();
		}
	});
});
