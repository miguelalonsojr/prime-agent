import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";

const shellMocks = vi.hoisted(() => ({
	cwdPollingSupported: vi.fn(() => true),
	runShellSession: vi.fn(),
}));
vi.mock("../src/modes/interactive/shell-session.js", () => shellMocks);

import { InteractiveMode } from "../src/modes/interactive/interactive-mode.js";

type TermHarness = {
	hasInterruptibleWork(): boolean;
	getDisplayCwd(): string;
	getCurrentCwd(): string;
	echoLocalCommand(text: string): void;
	showWarning(message: string): void;
	showError(message: string): void;
	showStatus(message: string): void;
	uiServices: { settingsManager: { getShellPath(): string | undefined } };
	agentConnection: { setKernelCwd(cwd: string): Promise<void> };
	connectionState: { kernelCwd?: string };
	ui: {
		terminal: { drainInput(timeout: number): Promise<void> };
		stop(): void;
		start(): void;
		requestRender(force?: boolean): void;
	};
	fullscreenEnabled: boolean;
	applyFullscreen(enabled: boolean): void;
	handleTermCommand(commandText: string): Promise<void>;
};

function createHarness(cwd: string, overrides: Partial<TermHarness> = {}): TermHarness {
	return Object.assign(Object.create(InteractiveMode.prototype), {
		hasInterruptibleWork: () => false,
		getDisplayCwd: () => cwd,
		getCurrentCwd: () => cwd,
		echoLocalCommand: vi.fn(),
		showWarning: vi.fn(),
		showError: vi.fn(),
		showStatus: vi.fn(),
		uiServices: { settingsManager: { getShellPath: () => "/bin/sh" } },
		agentConnection: { setKernelCwd: vi.fn(async () => {}) },
		connectionState: { kernelCwd: cwd },
		ui: { terminal: { drainInput: vi.fn(async () => {}) }, stop: vi.fn(), start: vi.fn(), requestRender: vi.fn() },
		fullscreenEnabled: true,
		applyFullscreen: vi.fn(),
		...overrides,
	}) as TermHarness;
}

const prototype = InteractiveMode.prototype as unknown as {
	handleTermCommand(this: TermHarness, commandText: string): Promise<void>;
};

afterEach(() => {
	vi.restoreAllMocks();
	shellMocks.cwdPollingSupported.mockReset().mockReturnValue(true);
	shellMocks.runShellSession.mockReset();
});

describe("InteractiveMode /term", () => {
	it("warns while work is interruptible", async () => {
		const harness = createHarness(tmpdir(), { hasInterruptibleWork: () => true });
		await prototype.handleTermCommand.call(harness, "/term");
		expect(harness.showWarning).toHaveBeenCalledWith("Wait for the current work to finish before opening a shell.");
		expect(shellMocks.runShellSession).not.toHaveBeenCalled();
		expect(harness.ui.stop).not.toHaveBeenCalled();
	});

	it("warns on Windows", async () => {
		const platform = vi.spyOn(process, "platform", "get").mockReturnValue("win32");
		const harness = createHarness(tmpdir());
		try {
			await prototype.handleTermCommand.call(harness, "/term");
			expect(harness.showWarning).toHaveBeenCalledWith("/term is not supported on Windows.");
			expect(shellMocks.runShellSession).not.toHaveBeenCalled();
		} finally {
			platform.mockRestore();
		}
	});

	it("propagates a changed valid shell cwd and restores the TUI", async () => {
		const oldDir = mkdtempSync(join(tmpdir(), "term-old-"));
		const nextDir = mkdtempSync(join(tmpdir(), "term-next-"));
		try {
			shellMocks.runShellSession.mockResolvedValue({ exitCode: 0, lastObservedCwd: nextDir });
			const harness = createHarness(oldDir);
			await prototype.handleTermCommand.call(harness, "/term");
			expect(shellMocks.runShellSession).toHaveBeenCalledWith({ shell: "/bin/sh", cwd: oldDir });
			expect(harness.ui.terminal.drainInput).toHaveBeenCalled();
			expect(harness.ui.stop).toHaveBeenCalledOnce();
			expect(harness.agentConnection.setKernelCwd).toHaveBeenCalledWith(nextDir);
			expect(harness.ui.start).toHaveBeenCalledOnce();
			expect(harness.applyFullscreen).toHaveBeenCalledWith(true);
			expect(harness.ui.requestRender).toHaveBeenCalledWith(true);
		} finally {
			rmSync(oldDir, { recursive: true, force: true });
			rmSync(nextDir, { recursive: true, force: true });
		}
	});

	it("warns when propagation fails without changing the snapshot", async () => {
		const oldDir = mkdtempSync(join(tmpdir(), "term-old-"));
		const nextDir = mkdtempSync(join(tmpdir(), "term-next-"));
		try {
			shellMocks.runShellSession.mockResolvedValue({ exitCode: 0, lastObservedCwd: nextDir });
			const harness = createHarness(oldDir, {
				agentConnection: {
					setKernelCwd: vi.fn(async () => {
						throw new Error("no");
					}),
				},
			});
			await prototype.handleTermCommand.call(harness, "/term");
			expect(harness.showWarning).toHaveBeenCalledWith(
				"The shell moved locally but could not be propagated to the agent.",
			);
			expect(harness.connectionState.kernelCwd).toBe(oldDir);
		} finally {
			rmSync(oldDir, { recursive: true, force: true });
			rmSync(nextDir, { recursive: true, force: true });
		}
	});

	it("reports shell errors and restores the TUI", async () => {
		shellMocks.runShellSession.mockRejectedValue(new Error("failed"));
		const harness = createHarness(tmpdir());
		await prototype.handleTermCommand.call(harness, "/term");
		expect(harness.showError).toHaveBeenCalledWith("failed");
		expect(harness.ui.start).toHaveBeenCalledOnce();
		expect(harness.agentConnection.setKernelCwd).not.toHaveBeenCalled();
	});
});
