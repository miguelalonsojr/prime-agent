import { describe, expect, it } from "vitest";
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
});
