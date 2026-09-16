import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, readlinkSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";

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

interface ShellLaunch {
	args: string[];
	env: NodeJS.ProcessEnv;
	cwdFile: string;
	cleanup(): void;
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

function writeHook(path: string, content: string): void {
	writeFileSync(path, content, { encoding: "utf8", mode: 0o600 });
}

function linkZshConfig(sourceDir: string | undefined, targetDir: string, name: string): void {
	if (!sourceDir) return;
	const source = join(sourceDir, name);
	if (existsSync(source)) symlinkSync(source, join(targetDir, name));
}

function createShellLaunch(options: ShellSessionOptions): ShellLaunch {
	const directory = mkdtempSync(join(tmpdir(), "prime-agent-term-"));
	const cwdFile = join(directory, "cwd");
	const hookFile = join(directory, "cwd-hook");
	const shellName = basename(options.shell).toLowerCase();
	const args = [...(options.shellArgs ?? [])];
	const env: NodeJS.ProcessEnv = {
		...process.env,
		PRIME_AGENT_TERM_CWD_FILE: cwdFile,
	};
	const reportCwd = 'pwd -P > "$PRIME_AGENT_TERM_CWD_FILE"';

	if (shellName === "bash") {
		writeHook(
			hookFile,
			`if [ -n "\${PRIME_AGENT_ORIGINAL_BASH_ENV:-}" ] && [ -r "$PRIME_AGENT_ORIGINAL_BASH_ENV" ]; then . "$PRIME_AGENT_ORIGINAL_BASH_ENV"; fi
case $- in *i*) [ ! -r "$HOME/.bashrc" ] || . "$HOME/.bashrc" ;; esac
trap '${reportCwd}' EXIT
`,
		);
		env.PRIME_AGENT_ORIGINAL_BASH_ENV = process.env.BASH_ENV;
		env.BASH_ENV = hookFile;
		args.unshift("--rcfile", hookFile);
	} else if (shellName === "zsh") {
		const originalZdotdir = process.env.ZDOTDIR ?? process.env.HOME;
		writeHook(
			join(directory, ".zshenv"),
			`if [[ -n "\${PRIME_AGENT_ORIGINAL_ZDOTDIR:-}" && -r "$PRIME_AGENT_ORIGINAL_ZDOTDIR/.zshenv" ]]; then source "$PRIME_AGENT_ORIGINAL_ZDOTDIR/.zshenv"; fi
autoload -Uz add-zsh-hook
function __prime_agent_report_cwd() { ${reportCwd}; }
add-zsh-hook zshexit __prime_agent_report_cwd
`,
		);
		for (const name of [".zprofile", ".zshrc", ".zlogin", ".zlogout"]) {
			linkZshConfig(originalZdotdir, directory, name);
		}
		env.PRIME_AGENT_ORIGINAL_ZDOTDIR = originalZdotdir;
		env.ZDOTDIR = directory;
	} else if (shellName === "fish") {
		args.unshift(
			"--init-command",
			'function __prime_agent_report_cwd --on-event fish_exit; pwd -P > "$PRIME_AGENT_TERM_CWD_FILE"; end',
		);
	} else if (["sh", "dash", "ksh", "mksh"].includes(shellName)) {
		writeHook(
			hookFile,
			`if [ -n "\${PRIME_AGENT_ORIGINAL_ENV:-}" ] && [ -r "$PRIME_AGENT_ORIGINAL_ENV" ]; then . "$PRIME_AGENT_ORIGINAL_ENV"; fi
trap '${reportCwd}' 0
`,
		);
		env.PRIME_AGENT_ORIGINAL_ENV = process.env.ENV;
		env.ENV = hookFile;
		const commandIndex = args.indexOf("-c");
		if (commandIndex >= 0 && args[commandIndex + 1] !== undefined) {
			args[commandIndex + 1] = `trap '${reportCwd}' 0
${args[commandIndex + 1]}`;
		}
	}

	return {
		args,
		env,
		cwdFile,
		cleanup: () => rmSync(directory, { recursive: true, force: true }),
	};
}

function readReportedCwd(path: string): string | null {
	try {
		const cwd = readFileSync(path, "utf8").replace(/\r?\n$/, "");
		return cwd || null;
	} catch {
		return null;
	}
}

export function runShellSession(options: ShellSessionOptions): Promise<ShellSessionResult> {
	return new Promise((resolvePromise, rejectPromise) => {
		const launch = createShellLaunch(options);
		const child = spawn(options.shell, launch.args, {
			stdio: "inherit",
			cwd: options.cwd,
			env: launch.env,
		});
		let lastObservedCwd: string | null = null;
		let poller: ReturnType<typeof setInterval> | undefined;
		const clearPoller = () => {
			if (poller) clearInterval(poller);
			poller = undefined;
		};
		const observeCwd = () => {
			if (child.pid === undefined) return;
			const cwd = readProcessCwd(child.pid);
			if (cwd) lastObservedCwd = cwd;
		};
		child.once("spawn", () => {
			observeCwd();
			poller = setInterval(observeCwd, options.pollIntervalMs ?? 300);
		});
		child.once("error", (error) => {
			clearPoller();
			launch.cleanup();
			rejectPromise(error);
		});
		child.once("close", (exitCode) => {
			clearPoller();
			const reportedCwd = readReportedCwd(launch.cwdFile);
			launch.cleanup();
			resolvePromise({ exitCode, lastObservedCwd: reportedCwd ?? lastObservedCwd });
		});
	});
}
