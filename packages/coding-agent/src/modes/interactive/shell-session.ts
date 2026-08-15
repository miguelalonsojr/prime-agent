import { spawn, spawnSync } from "node:child_process";
import { readlinkSync } from "node:fs";

/**
 * Read a live process's current working directory. Shell-agnostic: works for
 * bash/zsh/fish because it inspects the OS process, not shell internals.
 */
export function readProcessCwd(pid: number): string | null {
	if (process.platform === "linux") {
		try {
			return readlinkSync(`/proc/${pid}/cwd`);
		} catch {
			return null;
		}
	}
	if (process.platform === "darwin") {
		const result = spawnSync("lsof", ["-a", "-p", String(pid), "-d", "cwd", "-Fn"], {
			encoding: "utf8",
			stdio: ["ignore", "pipe", "ignore"],
		});
		if (result.status !== 0 || !result.stdout) return null;
		const line = result.stdout.split("\n").find((entry) => entry.startsWith("n"));
		return line ? line.slice(1) : null;
	}
	return null;
}

export function cwdPollingSupported(): boolean {
	return process.platform === "linux" || process.platform === "darwin";
}

export interface ShellSessionOptions {
	shell: string;
	shellArgs?: string[];
	cwd: string;
	pollIntervalMs?: number;
}

export interface ShellSessionResult {
	exitCode: number | null;
	/** Last cwd observed while the shell ran; null if never observed. */
	lastObservedCwd: string | null;
}

/**
 * Run an interactive shell attached to this terminal, polling its cwd so the
 * caller can adopt the directory the user ended in. The caller must stop the
 * TUI before calling and restart it after.
 */
export function runShellSession(options: ShellSessionOptions): Promise<ShellSessionResult> {
	return new Promise((resolvePromise, rejectPromise) => {
		const child = spawn(options.shell, options.shellArgs ?? [], {
			stdio: "inherit",
			cwd: options.cwd,
		});
		let lastObservedCwd: string | null = null;
		let poller: ReturnType<typeof setInterval> | undefined;

		child.once("spawn", () => {
			poller = setInterval(() => {
				if (child.pid === undefined) return;
				const cwd = readProcessCwd(child.pid);
				if (cwd) {
					lastObservedCwd = cwd;
				}
			}, options.pollIntervalMs ?? 300);
		});
		child.once("error", (error) => {
			if (poller) clearInterval(poller);
			rejectPromise(error);
		});
		child.once("exit", (code) => {
			if (poller) clearInterval(poller);
			resolvePromise({ exitCode: code, lastObservedCwd });
		});
	});
}
