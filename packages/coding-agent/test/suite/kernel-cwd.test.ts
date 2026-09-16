import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import type { AgentSession } from "../../src/core/agent-session.js";
import { TERM_CWD_CHANGED_CUSTOM_TYPE } from "../../src/core/messages.js";
import { createHarness, type Harness } from "./harness.js";

async function executeIpython(session: AgentSession, code: string) {
	const tool = session.getToolDefinition("ipython");
	if (!tool) throw new Error("Missing IPython tool");
	const output = await tool.execute("kernel-cwd-test", { code }, undefined, undefined, {} as never);
	expect(output.details).toMatchObject({ status: "ok" });
	return output;
}

function nextKernelCwdEvent(session: AgentSession): Promise<{ type: "kernel_cwd_changed"; cwd: string }> {
	return new Promise((resolve) => {
		const unsubscribe = session.subscribe((event) => {
			if (event.type !== "kernel_cwd_changed") return;
			unsubscribe();
			resolve(event);
		});
	});
}

describe("kernel cwd session boundary", () => {
	const harnesses: Harness[] = [];
	afterEach(() => {
		while (harnesses.length) harnesses.pop()?.cleanup();
	});

	it("publishes changed cwd values observed through the public IPython tool boundary", async () => {
		const harness = await createHarness();
		harnesses.push(harness);
		const nextDir = mkdtempSync(join(tmpdir(), "prime-agent-observed-cwd-"));
		try {
			const changed = nextKernelCwdEvent(harness.session);
			await executeIpython(harness.session, `__import__("os").chdir(${JSON.stringify(nextDir)})`);

			await expect(changed).resolves.toEqual({ type: "kernel_cwd_changed", cwd: nextDir });
			expect(harness.session.kernelCwd).toBe(nextDir);
			await executeIpython(harness.session, `__import__("os").chdir(${JSON.stringify(nextDir)})`);
			expect(harness.eventsOfType("kernel_cwd_changed")).toEqual([{ type: "kernel_cwd_changed", cwd: nextDir }]);
		} finally {
			await harness.session.setKernelCwd(harness.tempDir).catch(() => {});
			rmSync(nextDir, { recursive: true, force: true });
		}
	});

	it("sets a validated preboot cwd and queues a hidden next-turn notice", async () => {
		const harness = await createHarness();
		harnesses.push(harness);
		const nextDir = mkdtempSync(join(tmpdir(), "prime-agent-session-cwd-"));
		try {
			await harness.session.setKernelCwd(nextDir);
			expect(harness.session.kernelCwd).toBe(nextDir);
			expect(harness.eventsOfType("kernel_cwd_changed")).toEqual([{ type: "kernel_cwd_changed", cwd: nextDir }]);
			expect(harness.session.getPendingNextTurnMessageSnapshots()).toContainEqual(
				expect.objectContaining({
					customType: TERM_CWD_CHANGED_CUSTOM_TYPE,
					display: false,
					details: { cwd: nextDir },
				}),
			);
			const output = await executeIpython(
				harness.session,
				'__import__("builtins").print(__import__("os").getcwd())',
			);
			expect(output.details).toMatchObject({ stdout: `${nextDir}\n` });

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
			await harness.session.setKernelCwd(harness.tempDir).catch(() => {});
			rmSync(nextDir, { recursive: true, force: true });
		}
	});

	it("changes a live kernel before publishing its cwd", async () => {
		const harness = await createHarness();
		harnesses.push(harness);
		const nextDir = mkdtempSync(join(tmpdir(), "prime-agent-live-cwd-"));
		try {
			const initialized = nextKernelCwdEvent(harness.session);
			await executeIpython(harness.session, "1");
			await initialized;
			const changed = nextKernelCwdEvent(harness.session);

			await harness.session.setKernelCwd(nextDir);

			await expect(changed).resolves.toEqual({ type: "kernel_cwd_changed", cwd: nextDir });
			const output = await executeIpython(
				harness.session,
				'__import__("builtins").print(__import__("os").getcwd())',
			);
			expect(output.details).toMatchObject({ stdout: `${nextDir}\n` });
		} finally {
			await harness.session.setKernelCwd(harness.tempDir).catch(() => {});
			rmSync(nextDir, { recursive: true, force: true });
		}
	});
});
