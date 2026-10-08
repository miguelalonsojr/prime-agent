import { beforeAll, describe, expect, it, vi } from "vitest";
import type { ClmStatus } from "../src/core/clm/live-context.js";
import { emptyUsage } from "../src/core/usage.js";
import { InteractiveMode } from "../src/modes/interactive/interactive-mode.js";
import { initTheme } from "../src/modes/interactive/theme/theme.js";

type SubmitContext = {
	defaultEditor: { onSubmit?: (text: string) => Promise<void> };
	editor: { getText: () => string; setText: (text: string) => void };
	[key: string]: unknown;
};

type Prototype = {
	setupEditorSubmitHandler(this: SubmitContext): void;
};

const prototype = InteractiveMode.prototype as unknown as Prototype;

function makeSubmitContext() {
	const prompt = vi.fn(async () => undefined);
	let editorText = "";
	const context: SubmitContext = {
		defaultEditor: {},
		editor: {
			getText: () => editorText,
			setText: (text: string) => {
				editorText = text;
			},
		},
		agentConnection: { prompt },
		showError: vi.fn(),
		showStatus: vi.fn(),
		echoLocalCommand: vi.fn(),
		handleSessionCommand: vi.fn(async () => undefined),
		showTreeSelector: vi.fn(async () => undefined),
		showSettingsSelector: vi.fn(async () => undefined),
		showHeartbeatManager: vi.fn(async () => undefined),
		submittedInputBehavior: "steer",
		inputSubmissionGeneration: 0,
		inputSubmissionsPending: 0,
		pendingPromptStashReleases: [],
		promptStashState: {},
		pendingSubmittedPromptStash: undefined,
		snapshotPromptStash: vi.fn(() => ({ text: "" })),
		promptStash: undefined,
		promptStashSessionId: "session-1",
		sessionId: "session-1",
		clearShortcutGuide: vi.fn(),
	};
	return context;
}

describe("InteractiveMode no-argument command usage errors", () => {
	beforeAll(() => initTheme("dark"));

	it("rejects arguments to /tree with a usage error instead of prompting", async () => {
		const context = makeSubmitContext();
		const prompt = (context.agentConnection as { prompt: ReturnType<typeof vi.fn> }).prompt;
		prototype.setupEditorSubmitHandler.call(context);
		// The real editor clears its buffer before onSubmit runs; the usage
		// error must put the draft back for editing (as /clear does).
		// The real editor clears its buffer BEFORE invoking onSubmit.
		context.editor.setText("");
		await context.defaultEditor.onSubmit?.("/tree fix the bug");
		expect(context.showError).toHaveBeenCalledWith("Usage: /tree");
		expect(prompt).not.toHaveBeenCalled();
		expect(context.showTreeSelector).not.toHaveBeenCalled();
		expect(context.editor.getText()).toBe("/tree fix the bug");
	});

	it("still opens the tree selector without arguments", async () => {
		const context = makeSubmitContext();
		prototype.setupEditorSubmitHandler.call(context);
		await context.defaultEditor.onSubmit?.("/tree");
		expect(context.showTreeSelector).toHaveBeenCalledTimes(1);
		expect(context.showError).not.toHaveBeenCalled();
	});

	it.each(["settings", "session", "heartbeats"])("rejects arguments to /%s", async (command) => {
		const context = makeSubmitContext();
		const prompt = (context.agentConnection as { prompt: ReturnType<typeof vi.fn> }).prompt;
		prototype.setupEditorSubmitHandler.call(context);
		await context.defaultEditor.onSubmit?.(`/${command} stray argument`);
		expect(context.showError).toHaveBeenCalledWith(`Usage: /${command}`);
		expect(prompt).not.toHaveBeenCalled();
	});

	it("toggles /speed on and off, parses explicit args, and rejects invalid arguments", async () => {
		const context = makeSubmitContext();
		Object.setPrototypeOf(context, InteractiveMode.prototype); // runs the real setSpeedDisplay
		const footer = { setSpeedEnabled: vi.fn(), setSpeedText: vi.fn() };
		Object.assign(context, { footer, speedDisplayEnabled: false });
		Object.assign(context, { ui: { requestRender: vi.fn() }, uiServices: { settingsManager: {} } });
		prototype.setupEditorSubmitHandler.call(context);
		await context.defaultEditor.onSubmit?.("/speed");
		expect(footer.setSpeedEnabled).toHaveBeenLastCalledWith(true);
		await context.defaultEditor.onSubmit?.("/speed off");
		expect(footer.setSpeedEnabled).toHaveBeenLastCalledWith(false);
		expect(footer.setSpeedText).toHaveBeenCalledWith(undefined);
		await context.defaultEditor.onSubmit?.("/speed banana");
		expect(context.showError).toHaveBeenCalledWith("Usage: /speed [on|off]");
		expect((context.agentConnection as { prompt: ReturnType<typeof vi.fn> }).prompt).not.toHaveBeenCalled();
	});
});
describe("CLM interactive commands", () => {
	beforeAll(() => initTheme("dark"));
	function setup(overrides: Partial<ClmStatus> = {}) {
		const context = makeSubmitContext();
		Object.setPrototypeOf(context, InteractiveMode.prototype);
		const calls: string[] = [];
		const status: ClmStatus = {
			mode: "on",
			configuredDefault: false,
			modeSource: "session-override",
			path: "/tmp/clm/context.txt",
			epochId: "epoch",
			revision: 2,
			recentRevisions: [],
			...overrides,
		};
		const usage = emptyUsage();
		usage.input = 7;
		usage.cost.total = 0.25;
		const tree = {
			id: "root",
			label: "agent",
			status: "active" as const,
			ownUsage: usage,
			totalUsage: usage,
			children: [],
		};
		const connection = {
			prompt: vi.fn(),
			startSideQuestion: vi.fn(),
			getClmStatus: vi.fn(async () => status),
			setClmMode: vi.fn(async () => status),
			restoreClmRevision: vi.fn(async () => status),
			getMessages: vi.fn(async () => [{ role: "user", content: "deleted audit and pending bytes" }]),
			getClmModelSnapshot: vi.fn(async () => ({
				epochId: "epoch",
				revision: 2,
				messages: [{ role: "user", content: "accepted text" }],
			})),
			getContextTree: vi.fn(async () => tree),
		};
		Object.assign(context, {
			showWarning: vi.fn(),
			sideQuestionContainer: { clear: vi.fn() },
			uiServices: { settingsManager: {} },
			agentConnection: connection,
			ui: { terminal: { columns: 120 }, requestRender: vi.fn() },
			chatContainer: {
				addChild: (component: { render?: (width: number) => string[] }) => {
					if (component.render) calls.push(component.render(120).join("\n"));
				},
			},
		});
		prototype.setupEditorSubmitHandler.call(context);
		return { context, connection, calls, status, submit: (text: string) => context.defaultEditor.onSubmit!(text) };
	}
	it("shows accepted revision only and never reads pending file bytes", async () => {
		const { submit, calls } = setup();
		await submit("/clm view");
		expect(calls.join("\n")).toContain("epoch:2");
		expect(calls.join("\n")).toContain("user: accepted text");
		expect(calls.join("\n")).not.toContain("deleted audit and pending bytes");
	});
	it("renders accepted terminal controls as inert text while keeping newlines", async () => {
		const { submit, connection, calls } = setup();
		connection.getClmModelSnapshot.mockResolvedValueOnce({
			epochId: "epoch",
			revision: 2,
			messages: [{ role: "user", content: "top\n\x1b[2J\x1b[H\x1b]52;c;YQ==\x07bottom\x9b2J" }],
		});
		await submit("/clm view");
		const rendered = calls.join("\n");
		expect(rendered).toMatch(/user: top[^\n]*\n/);
		expect(rendered).toContain("bottom");
		expect(rendered).not.toMatch(/[\x00-\x08\x0b-\x1f\x7f-\x9f]/);
	});
	it("accepts revision zero and rejects invalid restore targets before the adapter", async () => {
		const { submit, connection, context, calls } = setup();
		for (const target of ["bad", "epoch:-1", "epoch:9007199254740992"]) {
			await submit(`/clm restore ${target}`);
			expect(context.showError).toHaveBeenCalledWith(expect.stringContaining("Usage: /clm"));
		}
		expect(connection.restoreClmRevision).not.toHaveBeenCalled();
		await submit("/clm restore epoch:0");
		expect(connection.restoreClmRevision).toHaveBeenCalledWith("epoch", 0);
		expect(calls.join("\n")).toContain("Accepted revision: epoch:2");
		expect(connection.prompt).not.toHaveBeenCalled();
	});
	it("keeps old-daemon errors local and the audit report usable", async () => {
		const { submit, connection, calls, context } = setup();
		connection.getClmStatus.mockRejectedValueOnce(new Error("CLM unsupported by old daemon"));
		await submit("/clm status");
		expect(context.showError).toHaveBeenCalledWith(expect.stringContaining("unsupported"));
		expect(connection.prompt).not.toHaveBeenCalled();
		connection.getClmStatus.mockRejectedValueOnce(new Error("old daemon"));
		await submit("/context");
		expect(calls.join("\n")).toContain("$0.25");
	});
	it("labels estimates separately from observed usage and unknown legacy attribution", async () => {
		const { submit, connection, calls, status } = setup({
			estimatedOccupancy: { estimatedInputTokens: 310, outputReserveTokens: 90, maxInputTokens: 410 },
			measuredUsage: { input: 16, output: 3, cacheRead: 2, cacheWrite: 1, cost: 0.04 },
		});
		await submit("/context");
		expect(calls.join("\n")).toMatch(/Estimated occupancy: 310 tokens/);
		expect(calls.join("\n")).toContain("Estimated input budget: 410 tokens");
		expect(calls.join("\n")).toContain("Estimated output reserve: 90 tokens");
		expect(calls.join("\n")).toMatch(/Observed billed usage.*16 input.*\$0.04/);
		connection.getClmStatus.mockResolvedValueOnce({ ...status, measuredUsage: undefined, modeSource: "unknown" });
		await submit("/context");
		expect(calls.at(-1)).toContain("Observed billed usage (current revision): unknown");
	});
	it("does not present invalid receipts as billed usage", async () => {
		const { submit, calls } = setup({
			measuredUsage: { input: -1, output: 0, cacheRead: 0, cacheWrite: 0, cost: NaN },
		});
		await submit("/context");
		expect(calls.at(-1)).toContain("Observed billed usage (current revision): unknown");
	});
	it("rejects a CLM-on side question before provider work", async () => {
		const { context, connection } = setup();
		await (
			InteractiveMode.prototype as unknown as {
				handleSideQuestion(this: SubmitContext, question: string): Promise<void>;
			}
		).handleSideQuestion.call(context, "why?");
		expect(context.showWarning).toHaveBeenCalledWith(expect.stringContaining("CLM is on"));
		expect(connection.startSideQuestion).not.toHaveBeenCalled();
	});
	it("warns about older binaries when enabling CLM", async () => {
		const { submit, connection, context } = setup();
		await submit("/clm on");
		expect(connection.setClmMode).toHaveBeenCalledWith(true);
		expect(context.showWarning).toHaveBeenCalledWith(expect.stringContaining("Older Prime Agent binaries"));
	});
});
