import { spawn, spawnSync } from "node:child_process";
import { readlinkSync } from "node:fs";

export interface ShellSessionOptions {
	shell: string;
	shellArgs?: string[];
	cwd: string;
	pollIntervalMs?: number;
}

export interface ShellSessionResult {
	exitCode: number | null;
	lastObservedCwd: string | null;
}

export function readProcessCwd(pid: number): string | null {
	if (process.platform === "linux") {
		try {
			return readlinkSync(`/proc/${pid}/cwd`);
		} catch {
			return null;
		}
	}
	if (process.platform === "darwin") {
		const result = spawnSync("lsof", ["-a", "-p", String(pid), "-d", "cwd", "-Fn"], { encoding: "utf8" });
		if (result.status !== 0) return null;
		const line = result.stdout.split("\n").find((value) => value.startsWith("n"));
		return line ? line.slice(1) : null;
	}
	return null;
}

export function cwdPollingSupported(platform: NodeJS.Platform = process.platform): boolean {
	return platform === "linux" || platform === "darwin";
}

export function runShellSession(options: ShellSessionOptions): Promise<ShellSessionResult> {
	return new Promise((resolvePromise, rejectPromise) => {
		const child = spawn(options.shell, options.shellArgs ?? [], { stdio: "inherit", cwd: options.cwd });
		let lastObservedCwd: string | null = null;
		let poller: ReturnType<typeof setInterval> | undefined;
		const clearPoller = () => {
			if (poller) clearInterval(poller);
			poller = undefined;
		};
		child.once("spawn", () => {
			poller = setInterval(() => {
				if (child.pid === undefined) return;
				const cwd = readProcessCwd(child.pid);
				if (cwd) lastObservedCwd = cwd;
			}, options.pollIntervalMs ?? 300);
		});
		child.once("error", (error) => {
			clearPoller();
			rejectPromise(error);
		});
		child.once("exit", (exitCode) => {
			clearPoller();
			resolvePromise({ exitCode, lastObservedCwd });
		});
	});
}
