import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, realpathSync, rmSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, describe, expect, it } from "vitest";
import { IpythonKernelProvisioner } from "../src/core/tools/ipython.js";

function resolveKernelPython(): string | null {
	const candidates = [
		process.env.PRIME_AGENT_KERNEL_PYTHON,
		join(homedir(), ".prime", "agent", "kernel-venv", "bin", "python"),
	].filter((p): p is string => Boolean(p));
	for (const python of candidates) {
		if (!existsSync(python)) continue;
		const check = spawnSync(python, ["-c", "import ipykernel"], { encoding: "utf8" });
		if (check.status === 0) return python;
	}
	return null;
}

const python = resolveKernelPython();
const describeIfKernel = python ? describe : describe.skip;

describeIfKernel("IpythonKernelProvisioner cwd API (real kernel)", { tags: ["kernel-heavy"] }, () => {
	const dirA = realpathSync(mkdtempSync(join(tmpdir(), "kernel-cwd-a-")));
	const dirB = realpathSync(mkdtempSync(join(tmpdir(), "kernel-cwd-b-")));
	const provisioners: IpythonKernelProvisioner[] = [];

	afterAll(async () => {
		for (const p of provisioners) await p.dispose();
		rmSync(dirA, { recursive: true, force: true });
		rmSync(dirB, { recursive: true, force: true });
	});

	it("returns null before the kernel boots, then reads and changes the kernel cwd", async () => {
		const provisioner = new IpythonKernelProvisioner(dirA, { python: python as string });
		provisioners.push(provisioner);

		expect(await provisioner.readCwd()).toBeNull();
		expect(await provisioner.chdir(dirB)).toBeNull();

		await provisioner.ensure();
		expect(await provisioner.readCwd()).toBe(dirA);
		expect(await provisioner.chdir(dirB)).toBe(dirB);
		expect(await provisioner.readCwd()).toBe(dirB);
		await expect(provisioner.chdir(join(dirB, "does-not-exist"))).rejects.toThrow();
	});

	it("boots into a cwd set before first boot", async () => {
		const provisioner = new IpythonKernelProvisioner(dirA, { python: python as string });
		provisioners.push(provisioner);
		provisioner.setCwd(dirB);
		await provisioner.ensure();
		expect(await provisioner.readCwd()).toBe(dirB);
	});
});
