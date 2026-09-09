import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ExecuteResult } from "../src/core/kernel/index.js";
import { createIpythonToolDefinition, IpythonKernelProvisioner } from "../src/core/tools/ipython.js";

function resolveReplPython(): string | null {
	const candidates = [
		process.env.PRIME_AGENT_KERNEL_PYTHON,
		resolve(__dirname, "..", "..", "..", "prime-agent-runtime", ".venv", "bin", "python"),
		join(homedir(), ".prime", "agent", "kernel-venv", "bin", "python"),
	].filter((candidate): candidate is string => Boolean(candidate));
	for (const python of candidates) {
		if (!existsSync(python)) continue;
		if (spawnSync(python, ["-c", "import rlm.repl, dill"], { encoding: "utf8" }).status === 0) return python;
	}
	return null;
}

const python = resolveReplPython();
const describeIf = python ? describe : describe.skip;

describeIf("IpythonKernelProvisioner cwd (real runtime)", () => {
	let dirA = "";
	let dirB = "";
	let provisioner: IpythonKernelProvisioner | undefined;

	afterEach(async () => {
		await provisioner?.dispose({ snapshot: false });
		provisioner = undefined;
		if (dirA) rmSync(dirA, { recursive: true, force: true });
		if (dirB) rmSync(dirB, { recursive: true, force: true });
		dirA = "";
		dirB = "";
	});

	it("returns null before boot", async () => {
		dirA = mkdtempSync(join(tmpdir(), "prime-agent-kernel-cwd-a-"));
		provisioner = new IpythonKernelProvisioner(dirA, { python: python as string });
		expect(await provisioner.readCwd()).toBeNull();
		expect(await provisioner.chdir(dirA)).toBeNull();
	});

	it("uses a preboot cwd and tracks a live change without snapshots", async () => {
		dirA = mkdtempSync(join(tmpdir(), "prime-agent-kernel-cwd-a-"));
		dirB = mkdtempSync(join(tmpdir(), "prime-agent-kernel-cwd-b-"));
		provisioner = new IpythonKernelProvisioner(dirA, { python: python as string });
		provisioner.setCwd(dirB);
		const manager = await provisioner.ensure();
		expect(await provisioner.readCwd()).toBe(dirB);
		const scheduleSnapshot = vi.spyOn(manager as unknown as { scheduleSnapshot(): void }, "scheduleSnapshot");
		await manager.execute("1");
		expect(scheduleSnapshot).toHaveBeenCalledOnce();
		scheduleSnapshot.mockClear();
		expect(await provisioner.chdir(dirA)).toBe(dirA);
		expect(await provisioner.readCwd()).toBe(dirA);
		expect(scheduleSnapshot).not.toHaveBeenCalled();
		await expect(provisioner.chdir(join(dirA, "missing"))).rejects.toThrow();
	});
});

function settledResult(status: "ok" | "error"): ExecuteResult {
	return status === "ok"
		? { stdout: "settled", stderr: "", status, durationMs: 1 }
		: { stdout: "", stderr: "", status, durationMs: 1, error: { ename: "Error", evalue: "failed", traceback: [] } };
}

describe("Ipython tool execution observer", () => {
	it.each(["ok", "error"] as const)("observes settled %s transport results", async (status) => {
		const execute = vi.fn(async () => settledResult(status));
		const provisioner = { ensure: async () => ({ execute }) } as unknown as IpythonKernelProvisioner;
		const observer = vi.fn(() => {
			if (status === "ok") throw new Error("observer failed");
		});
		const tool = createIpythonToolDefinition("/tmp", { provisioner, onKernelExecutionSettled: observer });
		const output = await tool.execute("call", { code: "1" }, undefined, undefined, {} as never);
		expect(observer).toHaveBeenCalledOnce();
		expect(output.details).toMatchObject({ status, stdout: settledResult(status).stdout });
		expect(output.content).toEqual([{ type: "text", text: status === "ok" ? "settled" : "" }]);
	});
});
