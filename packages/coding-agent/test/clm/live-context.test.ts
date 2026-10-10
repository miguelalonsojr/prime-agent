import syncFileSystem, { existsSync } from "node:fs";
import fileSystem, { mkdtemp, readdir, readFile, rename, rm, writeFile } from "node:fs/promises";
import { syncBuiltinESMExports } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type Api, fauxAssistantMessage, fauxToolCall, type Model } from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it, vi } from "vitest";
import { renderContextDocument } from "../../src/core/clm/context-document.js";
import { LiveContextController } from "../../src/core/clm/live-context.js";
import { type ClmBlock, SessionManager } from "../../src/core/session-manager.js";
import { emptyUsage } from "../../src/core/usage.js";
import { createDeferred } from "../suite/scheduling.js";

const model: Model<Api> = {
	id: "local",
	name: "Local",
	api: "openai-responses",
	provider: "openai",
	baseUrl: "http://unused.invalid",
	reasoning: false,
	input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 1000,
	maxTokens: 300,
};
const budget = { systemPrompt: "fixed", tools: [], model };
const directories: string[] = [];
afterEach(async () => {
	await Promise.all(directories.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});
type AuditMessage = Parameters<SessionManager["appendMessage"]>[0];
const user = (content: string): AuditMessage => ({ role: "user", content, timestamp: 1 });
async function fixture(messages: AuditMessage[] = [user("obsolete"), user("current request")]) {
	const dir = await mkdtemp(join(tmpdir(), "clm-controller-"));
	directories.push(dir);
	const manager = SessionManager.create(dir, join(dir, "sessions"));
	const blocks = messages.map((message) => ({ id: manager.appendMessage(message), message }));
	manager.appendClmOn({ blocks }, blocks.at(-1)!.id);
	const controller = await LiveContextController.open(manager, dir);
	const protectedActions = new Map<string, readonly ClmBlock[]>([["request", [blocks.at(-1)!]]]);
	const request = { ...budget, protectedActions };
	const project = () => controller.projectRequest(new Set(protectedActions.keys()), request);
	const prepare = () => controller.prepareToolAction("tool", protectedActions);
	const settle = () => controller.settleToolAction("tool", protectedActions, budget);
	const path = controller.getClmStatus().path;
	const change = async (edit: (text: string) => string) => {
		await writeFile(`${path}.tmp`, edit(await readFile(path, "utf8")));
		await rename(`${path}.tmp`, path);
	};
	return { dir, manager, blocks, controller, protectedActions, request, project, prepare, settle, path, change };
}

describe("CLM live context", () => {
	it("attributes billed usage to the revision sent to the provider after reopen", async () => {
		const f = await fixture();
		const first = f.manager.captureClmUsagePin();
		const usage = emptyUsage();
		usage.cacheRead = 17;
		usage.cost.total = 0.23;
		f.manager.recordClmUsage(first, usage);
		await f.prepare();
		await f.change((text) => text.replace("obsolete", "summary"));
		await f.settle();
		expect(f.manager.getClmMeasuredUsage()).toBeUndefined();
		const second = f.manager.captureClmUsagePin();
		f.manager.recordClmUsage(second, { ...usage, cacheRead: 41, cost: { ...usage.cost, total: 0.81 } });
		expect(f.manager.getClmMeasuredUsage()).toEqual({
			input: 0,
			output: 0,
			cacheRead: 41,
			cacheWrite: 0,
			cost: 0.81,
		});
		f.manager.flushNow();
		const reopened = SessionManager.open(f.manager.getSessionFile()!);
		expect(reopened.getClmMeasuredUsage()).toEqual({ input: 0, output: 0, cacheRead: 41, cacheWrite: 0, cost: 0.81 });
	});
	it("treats unsupported and missing receipts as unknown", async () => {
		const f = await fixture();
		const pin = f.manager.captureClmUsagePin();
		expect(f.manager.getClmMeasuredUsage()).toBeUndefined();
		f.manager.recordClmUsage(pin, emptyUsage());
		expect(
			f.manager.getBranch().filter((entry) => entry.type === "custom" && entry.customType === "clm_usage_v1"),
		).toEqual([]);
		expect(
			f.manager
				.getBranch()
				.filter((entry) => entry.type === "custom" && entry.customType === "clm_usage_unknown_v1"),
		).toHaveLength(1);
		expect(f.manager.getClmMeasuredUsage()).toBeUndefined();
		f.manager.flushNow();
		expect(SessionManager.open(f.manager.getSessionFile()!).getClmMeasuredUsage()).toBeUndefined();
		f.manager.appendCustomEntry("clm_usage_v2", { version: 2, pin, usage: emptyUsage() });
		expect(f.manager.getClmMeasuredUsage()).toBeUndefined();
		f.manager.branch(pin.branchId!);
		f.manager.appendCustomEntry("clm_usage_v1", { version: 1, pin, usage: emptyUsage() });
		expect(f.manager.getClmMeasuredUsage()).toBeUndefined();
		f.manager.branch(pin.branchId!);
		f.manager.recordClmUsage(pin, { ...emptyUsage(), cost: { ...emptyUsage().cost, input: Number.NaN } });
		expect(f.manager.getClmMeasuredUsage()).toBeUndefined();
	});

	it("inspects oversized durable context without relaxing provider or mirror limits", async () => {
		const accepted = user("a".repeat(20 * 1024 * 1024));
		const later = user("b".repeat(14 * 1024 * 1024));
		const f = await fixture([accepted]);
		f.manager.appendMessage(later);
		f.manager.flushNow();
		await expect(f.project()).rejects.toThrow(/32 MiB/);
		await expect(f.prepare()).rejects.toThrow(/32 MiB/);
		await writeFile(f.path, "pending bytes");
		await expect(LiveContextController.readAcceptedView(f.manager)).resolves.toEqual({
			epochId: f.manager.getClmBranchState().epochId,
			revision: 0,
			messages: [accepted, later],
		});
		expect(await readFile(f.path, "utf8")).toBe("pending bytes");
	});

	it("pins a read behind restore without importing pending mirror bytes", async () => {
		const f = await fixture();
		await f.prepare();
		await f.change((text) => text.replace("obsolete", "summary"));
		await f.settle();
		const epoch = f.manager.getClmBranchState().epochId!;
		const restoring = f.controller.restoreRevision(epoch, 0);
		const reading = LiveContextController.readAcceptedView(f.manager);
		await restoring;
		expect(await reading).toEqual({ epochId: epoch, revision: 2, messages: f.blocks.map((block) => block.message) });
		f.blocks[0].message = user("pending edit");
		await writeFile(f.path, renderContextDocument({ blocks: f.blocks }, new Set()));
		expect(JSON.stringify(await LiveContextController.readAcceptedView(f.manager))).not.toContain("pending edit");
		expect(await readFile(f.path, "utf8")).toContain("pending edit");
		expect(f.manager.getClmBranchState().revision).toBe(2);
	});

	it("admits only the healthy owned writer and rejects cancellation or unknown markers", async () => {
		const f = await fixture();
		await f.prepare();
		expect(() => f.controller.assertChildAdmissionAvailable("tool")).not.toThrow();
		for (const id of [undefined, "other"]) expect(() => f.controller.assertChildAdmissionAvailable(id)).toThrow();
		f.controller.abortToolAction("tool", Promise.reject(new Error("unknown physical settlement")));
		expect(() => f.controller.assertChildAdmissionAvailable("tool")).toThrow(/writer/);
		await expect(f.controller.restoreRevision(f.manager.getClmBranchState().epochId!, 0)).rejects.toThrow(/writer/);
		const other = await fixture();
		await writeFile(`${other.manager.getSessionFile()}.clm-writer`, "unknown");
		expect(() => other.controller.assertChildAdmissionAvailable()).toThrow(/writer/);
	});

	it.each(["marker", "directory", "removal"])("fails closed when %s fsync fails", async (kind) => {
		const f = await fixture();
		if (kind === "removal") await f.prepare();
		let calls = 0;
		const original = syncFileSystem.fsyncSync;
		const fault = vi.spyOn(syncFileSystem, "fsyncSync").mockImplementation((fd) => {
			if (++calls === (kind === "directory" ? 2 : 1)) throw new Error("injected sync failure");
			original(fd);
		});
		syncBuiltinESMExports();
		try {
			await expect(kind === "removal" ? f.settle() : f.prepare()).rejects.toThrow(/marker|fsync/);
			await expect(f.project()).rejects.toThrow();
			expect(existsSync(`${f.manager.getSessionFile()}.clm-writer`)).toBe(true);
		} finally {
			fault.mockRestore();
			syncBuiltinESMExports();
		}
	});

	it("repairs only after the retained physical receipt settles and fences a new epoch", async () => {
		const f = await fixture();
		await f.prepare();
		const release = createDeferred();
		const physical = release.promise.then(() => writeFile(f.path, "late bytes"));
		f.controller.abortToolAction("tool", physical);
		try {
			await expect(f.controller.repairCancelledToolAction()).rejects.toThrow(/settlement/);
			expect(() => LiveContextController.assertWriterSettled(f.manager)).toThrow(/writer/);
		} finally {
			release.resolve();
			await physical;
		}
		await expect(f.project()).rejects.toThrow(/writer/);
		await f.controller.repairCancelledToolAction();
		expect(await readFile(f.path, "utf8")).not.toContain("late bytes");
		expect(await f.project()).toEqual(f.blocks.map((block) => block.message));
		f.manager.appendClmOffBoundary({ blocks: f.blocks }, f.blocks[1].id);
		f.manager.appendClmOn({ blocks: f.blocks }, f.blocks[1].id);
		const reopened = await LiveContextController.open(SessionManager.open(f.manager.getSessionFile()!), f.dir);
		expect(JSON.stringify(await reopened.projectRequest(new Set(["request"]), f.request))).not.toContain(
			"late bytes",
		);
	});

	it("cancels an import paused in its mirror read without committing or repairing", async () => {
		const f = await fixture();
		await f.prepare();
		await f.change((text) => text.replace("obsolete", "late bytes"));
		const [started, release] = [createDeferred(), createDeferred()];
		const original = fileSystem.open;
		const fault = vi.spyOn(fileSystem, "open").mockImplementation(async (...args) => {
			const file = await original(...args);
			if (args[0] === f.path) {
				started.resolve();
				await release.promise;
			}
			return file;
		});
		syncBuiltinESMExports();
		const settlement = expect(f.settle()).rejects.toThrow(/writer/);
		try {
			await started.promise;
			f.controller.abortToolAction("tool", Promise.resolve());
		} finally {
			release.resolve();
			try {
				await settlement;
			} finally {
				fault.mockRestore();
				syncBuiltinESMExports();
			}
		}
		expect(f.manager.getClmBranchState().revision).toBe(0);
		expect(await readFile(f.path, "utf8")).toContain("late bytes");
		await expect(f.project()).rejects.toThrow(/writer/);
	});

	it("does not republish an admitted writer's mirror on a fresh reopen", async () => {
		const f = await fixture();
		await f.prepare();
		await writeFile(f.path, "unsettled writer bytes");
		const reopened = SessionManager.open(f.manager.getSessionFile()!);
		await expect(LiveContextController.open(reopened, f.dir)).rejects.toThrow(/writer|settlement/i);
		expect(await readFile(f.path, "utf8")).toBe("unsettled writer bytes");
		await rm(join(f.dir, "clm"), { recursive: true });
		reopened.appendClmOffBoundary({ blocks: f.blocks }, f.blocks[1].id);
		reopened.appendClmOn({ blocks: f.blocks }, f.blocks[1].id);
		await expect(LiveContextController.open(SessionManager.open(f.manager.getSessionFile()!), f.dir)).rejects.toThrow(
			/writer/,
		);
	});

	it.each([false, true])(
		"reports durable acceptance after mirror publication failure (persistent=%s)",
		async (persistent) => {
			const f = await fixture();
			await f.prepare();
			await f.change((text) => text.replace("obsolete", "summary"));
			let committed = false;
			let failures = 0;
			const unsubscribe = f.manager.onPersist(() => {
				committed = true;
			});
			const originalRename = fileSystem.rename;
			const fault = vi.spyOn(fileSystem, "rename").mockImplementation(async (source, destination) => {
				if (committed && destination === f.path && (++failures === 1 || persistent))
					throw new Error("injected mirror publication failure");
				return originalRename(source, destination);
			});
			syncBuiltinESMExports();
			try {
				if (persistent) {
					await expect(f.settle()).rejects.toThrow(/revision 1.*accepted.*mirror.*repair/i);
					await expect(f.project()).rejects.toThrow(/revision 1.*accepted.*mirror.*repair/i);
				} else {
					expect((await f.settle())?.revision).toBe(1);
					expect(JSON.stringify(await f.project())).toContain("summary");
				}
				expect(f.controller.getClmStatus().revision).toBe(1);
				const durable = SessionManager.open(f.manager.getSessionFile()!).getClmBranchState();
				expect(durable.revision).toBe(1);
				expect(JSON.stringify(durable.snapshot)).toContain("summary");
				expect((await readdir(join(f.dir, "clm"))).filter((name) => name.includes(".rejected-"))).toEqual([]);
			} finally {
				fault.mockRestore();
				syncBuiltinESMExports();
				unsubscribe();
			}
		},
	);

	it("estimates an unknown model window without a hard gate", async () => {
		const f = await fixture();
		const unknown = { ...model };
		Reflect.deleteProperty(unknown, "contextWindow");
		await f.prepare();
		await f.change((text) => text.replace("obsolete", "x".repeat(4000)));
		await f.controller.settleToolAction("tool", f.protectedActions, { ...budget, model: unknown });
		expect((await f.controller.projectRequest(new Set(["request"]), { ...f.request, model: unknown })).length).toBe(
			2,
		);
		expect(f.controller.getClmStatus().estimatedOccupancy?.maxInputTokens).toBeNull();
	});

	it("refuses to cover a pending suffix with a null watermark", async () => {
		const f = await fixture();
		f.manager.appendClmOffBoundary({ blocks: f.blocks }, f.blocks[1].id);
		f.manager.appendClmOn({ blocks: f.blocks }, null);
		const call: AuditMessage = {
			role: "assistant",
			content: [{ type: "toolCall", id: "pending", name: "edit", arguments: {} }],
			api: model.api,
			provider: model.provider,
			model: model.id,
			usage: emptyUsage(),
			stopReason: "toolUse",
			timestamp: 2,
		};
		f.manager.appendMessage(call);
		const manager = SessionManager.open(f.manager.getSessionFile()!);
		const controller = await LiveContextController.open(manager, join(f.dir, "new-epoch"));
		await controller.prepareToolAction("edit", f.protectedActions);
		const path = controller.getClmStatus().path;
		await writeFile(path, (await readFile(path, "utf8")).replace("obsolete", "summary"));
		await expect(controller.settleToolAction("edit", f.protectedActions, budget)).rejects.toThrow(/watermark/);
		expect(manager.getClmBranchState().revision).toBe(0);
		expect(await controller.projectRequest(new Set(["request"]), f.request)).toEqual([
			...f.blocks.map((block) => block.message),
			call,
		]);
	});

	it.each([1, 2])("keeps a %s-call in-flight group outside revisions across reopen", async (count) => {
		const f = await fixture();
		const calls = Array.from({ length: count }, (_, index) => ({
			type: "toolCall" as const,
			id: `active-${index}`,
			name: "edit",
			arguments: { path: f.path },
		}));
		const call: AuditMessage = {
			role: "assistant",
			content: calls,
			api: model.api,
			provider: model.provider,
			model: model.id,
			usage: emptyUsage(),
			stopReason: "toolUse",
			timestamp: 2,
		};
		const receipts: AuditMessage[] = calls.map((item) => ({
			role: "toolResult",
			toolCallId: item.id,
			toolName: "edit",
			content: [{ type: "text", text: "saved" }],
			isError: false,
			timestamp: 3,
		}));
		f.manager.appendMessage(call);
		if (count === 2) f.manager.appendMessage(receipts[0]);
		await f.prepare();
		expect(await readFile(f.path, "utf8")).not.toContain("active-");
		await f.change((text) => text.replace("obsolete", "summary"));
		await f.settle();
		expect(f.manager.getClmBranchState().auditWatermark).toBe(f.blocks[1].id);
		expect(JSON.stringify(f.manager.getClmBranchState().snapshot)).not.toContain("active-");
		expect(f.manager.buildSessionContext().messages.slice(2)).toEqual([call, ...receipts.slice(0, count - 1)]);
		const manager = SessionManager.open(f.manager.getSessionFile()!);
		expect(manager.buildSessionContext().messages.slice(2)).toEqual([call, ...receipts.slice(0, count - 1)]);
		const reopened = await LiveContextController.open(manager, join(f.dir, "reopened"));
		manager.appendMessage(receipts[count - 1]);
		const projection = await reopened.projectRequest(new Set(["request"]), f.request);
		expect(projection.slice(2)).toEqual([call, ...receipts]);
		expect(JSON.stringify(projection)).not.toContain("obsolete");
		await reopened.prepareToolAction("next", f.protectedActions);
		expect(await readFile(reopened.getClmStatus().path, "utf8")).toContain("active-");
		await reopened.settleToolAction("next", f.protectedActions, budget);
	});

	it.each(["missing", "reordered", "orphan", "accepted"])("fails closed on %s native group history", async (kind) => {
		const f = await fixture();
		const call: AuditMessage = {
			role: "assistant",
			content: [0, 1, 2].map((index) => ({ type: "toolCall", id: `call-${index}`, name: "edit", arguments: {} })),
			api: model.api,
			provider: model.provider,
			model: model.id,
			usage: emptyUsage(),
			stopReason: "toolUse",
			timestamp: 2,
		};
		const receipt: AuditMessage = {
			role: "toolResult",
			toolCallId: kind === "reordered" ? "call-1" : "call-0",
			toolName: "edit",
			content: [],
			isError: false,
			timestamp: 3,
		};
		if (kind !== "orphan") f.manager.appendMessage(call);
		f.manager.appendMessage(receipt);
		if (kind === "missing") f.manager.appendMessage(user("unrelated request"));
		if (kind === "accepted") {
			f.manager.appendClmRevision(
				{ blocks: [...f.blocks, { id: "bad-call", message: call }] },
				f.manager.getLeafId(),
				"invalid-tail",
			);
			await expect(
				LiveContextController.open(SessionManager.open(f.manager.getSessionFile()!), join(f.dir, "reopened")),
			).rejects.toThrow();
		} else await expect(f.prepare()).rejects.toThrow();
	});

	it("rejects a document above 32 MiB without advancing the revision", async () => {
		const f = await fixture();
		await f.prepare();
		await f.change((text) => text.replace("obsolete", "x".repeat(32 * 1024 * 1024)));
		await expect(
			f.controller
				.settleToolAction("tool", f.protectedActions, {
					...budget,
					model: { ...model, contextWindow: 0 },
				})
				.then(() => undefined),
		).rejects.toThrow(/32 MiB/);
		expect(f.controller.getClmStatus().revision).toBe(0);
	});

	it("rejects the session snapshot cap before durable acceptance", async () => {
		const f = await fixture();
		await f.prepare();
		await f.change((text) => text.replace("obsolete", "summary"));
		const accepted = f.manager.getEntries().find((entry) => entry.type === "clm_on")!;
		// Bound memory while simulating a session with large older snapshot payloads.
		const oldSnapshot = { blocks: [{ id: "old", message: user("x".repeat(32 * 1024 * 1024)) }] };
		const read = vi
			.spyOn(f.manager, "getEntries")
			.mockReturnValue(Array.from({ length: 16 }, () => ({ ...accepted, snapshot: oldSnapshot })));
		try {
			await expect(f.settle()).rejects.toThrow(/512 MiB/);
			expect(f.manager.getClmBranchState().revision).toBe(0);
		} finally {
			read.mockRestore();
		}
	});

	it("accepts a cooperative edit durably and never replays the removed audit on reopen", async () => {
		const f = await fixture();
		await f.prepare();
		expect(await readFile(f.path, "utf8")).not.toContain("current request");
		await f.change((text) => text.replace("obsolete", "summary"));
		const revision = await f.settle();
		expect(revision?.revision).toBe(1);
		expect(JSON.stringify(await f.project())).toContain("summary");
		expect(JSON.stringify(await f.project())).not.toContain("obsolete");
		expect(JSON.stringify(f.manager.getEntries())).toContain("obsolete");
		await writeFile(f.path, "unaccepted bytes");
		const reopened = await LiveContextController.open(SessionManager.open(f.manager.getSessionFile()!), f.dir);
		expect(JSON.stringify(await reopened.projectRequest(new Set(["request"]), f.request))).toContain("summary");
		expect(JSON.stringify(await reopened.projectRequest(new Set(["request"]), f.request))).not.toContain(
			"unaccepted",
		);
	});

	it("accepts multiple durable child attributions during an edited tool action", async () => {
		const f = await fixture();
		const assistant = fauxAssistantMessage("parent tool call");
		const targetId = f.manager.appendMessage(assistant);
		await f.prepare();
		const first = { ...emptyUsage(), output: 3 };
		const total = { ...emptyUsage(), output: 8 };
		f.manager.appendChildUsageAttribution(targetId, first, first);
		f.manager.appendChildUsageAttribution(targetId, { ...emptyUsage(), output: 5 }, total);
		await f.change((text) => text.replace("obsolete", "summary"));
		expect((await f.settle())?.revision).toBe(1);
		f.manager.flushNow();
		const reopened = SessionManager.open(f.manager.getSessionFile()!);
		const attributions = reopened.getBranch().filter((entry) => entry.type === "child_usage_attributed");
		expect(attributions).toHaveLength(2);
		expect(attributions.map((entry) => entry.childUsage.output)).toEqual([3, 5]);
		expect(reopened.getEntry(targetId)).toMatchObject({ message: { usage: { output: 8 } } });
		expect(reopened.getClmBranchState()).toMatchObject({ revision: 1, auditWatermark: targetId });
		expect(JSON.stringify(reopened.getClmBranchState().snapshot)).toContain("summary");
	});

	it("admits child accounting while publishing the tool mirror", async () => {
		const f = await fixture();
		const targetId = f.manager.appendMessage(fauxAssistantMessage("parent tool call"));
		const started = createDeferred();
		const release = createDeferred();
		const original = fileSystem.rename;
		const fault = vi.spyOn(fileSystem, "rename").mockImplementation(async (from, to) => {
			await original(from, to);
			if (to === f.path) {
				started.resolve();
				await release.promise;
			}
		});
		syncBuiltinESMExports();
		try {
			const preparation = f.prepare();
			try {
				await started.promise;
				f.manager.appendChildUsageAttribution(targetId, emptyUsage(), emptyUsage());
			} finally {
				release.resolve();
			}
			await preparation;
			expect(await f.settle()).toBeNull();
		} finally {
			fault.mockRestore();
			syncBuiltinESMExports();
		}
	});

	it("clears the writer marker without a revision when only child accounting changed", async () => {
		const f = await fixture();
		const targetId = f.manager.appendMessage(fauxAssistantMessage("parent tool call"));
		await f.prepare();
		f.manager.appendChildUsageAttribution(targetId, emptyUsage(), emptyUsage());
		expect(await f.settle()).toBeNull();
		expect(f.manager.getClmBranchState().revision).toBe(0);
		expect(existsSync(`${f.manager.getSessionFile()}.clm-writer`)).toBe(false);
	});

	it("accepts child accounting during the awaited mirror read before committing", async () => {
		const f = await fixture();
		const targetId = f.manager.appendMessage(fauxAssistantMessage("parent tool call"));
		await f.prepare();
		await f.change((text) => text.replace("obsolete", "summary"));
		const started = createDeferred();
		const release = createDeferred();
		const original = fileSystem.open;
		const fault = vi.spyOn(fileSystem, "open").mockImplementation(async (...args) => {
			const file = await original(...args);
			if (args[0] === f.path) {
				started.resolve();
				await release.promise;
			}
			return file;
		});
		syncBuiltinESMExports();
		try {
			const settlement = f.settle();
			try {
				await started.promise;
				f.manager.appendChildUsageAttribution(targetId, emptyUsage(), emptyUsage());
			} finally {
				release.resolve();
			}
			expect((await settlement)?.revision).toBe(1);
		} finally {
			fault.mockRestore();
			syncBuiltinESMExports();
		}
	});

	it.each(["message", "custom"])("rejects %s after child accounting during a tool action", async (kind) => {
		const f = await fixture();
		const targetId = f.manager.appendMessage(fauxAssistantMessage("parent tool call"));
		await f.prepare();
		f.manager.appendChildUsageAttribution(targetId, emptyUsage(), emptyUsage());
		if (kind === "message") f.manager.appendMessage(user("unrelated"));
		else f.manager.appendCustomEntry("unrelated", {});
		await f.change((text) => text.replace("obsolete", "summary"));
		await expect(f.settle()).rejects.toThrow(/session changed during tool action/);
		expect(f.manager.getClmBranchState().revision).toBe(0);
		expect(existsSync(`${f.manager.getSessionFile()}.clm-writer`)).toBe(true);
	});

	it("rejects a branch that drops the captured leaf despite new child accounting", async () => {
		const f = await fixture();
		const priorLeaf = f.manager.getLeafId()!;
		const targetId = f.manager.appendMessage(fauxAssistantMessage("parent tool call"));
		await f.prepare();
		f.manager.branch(priorLeaf);
		f.manager.appendChildUsageAttribution(targetId, emptyUsage(), emptyUsage());
		await f.change((text) => text.replace("obsolete", "summary"));
		await expect(f.settle()).rejects.toThrow(/session changed during tool action/);
		expect(f.manager.getClmBranchState().revision).toBe(0);
	});

	it("preserves native tools without creating a revision when unchanged", async () => {
		const native: AuditMessage[] = [
			{
				role: "assistant",
				content: [{ type: "toolCall", id: "call", name: "read", arguments: { path: "a" } }],
				api: model.api,
				provider: model.provider,
				model: model.id,
				usage: emptyUsage(),
				stopReason: "toolUse",
				timestamp: 1,
			},
			{
				role: "toolResult",
				toolCallId: "call",
				toolName: "read",
				content: [{ type: "text", text: "loaded skill" }],
				isError: false,
				timestamp: 2,
			},
			user("current request"),
		];
		const f = await fixture(native);
		await f.prepare();
		expect(await f.settle()).toBeNull();
		expect(await f.project()).toEqual(native);
		expect(f.controller.getClmStatus().revision).toBe(0);
	});

	it.each(["missing", "relabelled", "edited", "malformed"])(
		"rejects %s protected references without accepting or losing rejected bytes",
		async (kind) => {
			const f = await fixture();
			await f.prepare();
			const marker = `[[CTX_PROTECTED id=${f.blocks[1].id}]]`;
			await f.change((text) =>
				kind === "missing"
					? text.replace(marker, "")
					: kind === "malformed"
						? `${text}junk`
						: text.replace(
								marker,
								`[[CTX_TURN id=${f.blocks[1].id} role=user]]\n${kind === "edited" ? "changed request" : "current request"}\n[[CTX_END]]`,
							),
			);
			const rejectedText = await readFile(f.path, "utf8");
			await expect(f.settle()).rejects.toThrow();
			const rejected = (await readdir(join(f.dir, "clm"))).find((name) => name.startsWith("context.txt.rejected-"));
			expect(rejected).toBeDefined();
			expect(await readFile(join(f.dir, "clm", rejected!), "utf8")).toBe(rejectedText);
			expect(f.controller.getClmStatus().revision).toBe(0);
			expect(await f.project()).toEqual(f.blocks.map((block) => block.message));
			expect(f.controller.getClmStatus().error).toBeTruthy();
		},
	);

	it("rejects an over-budget edit without advancing accepted revision", async () => {
		const f = await fixture();
		await f.prepare();
		await f.change((text) => text.replace("obsolete", "x".repeat(4000)));
		await expect(f.settle()).rejects.toThrow(/budget/i);
		expect(f.controller.getClmStatus().revision).toBe(0);
		expect(await f.project()).toEqual(f.blocks.map((block) => block.message));
	});

	it("rejects over-budget requests and keeps a nonaccumulating near-budget status", async () => {
		const f = await fixture();
		const near = { ...f.request, systemPrompt: "x".repeat(2300) };
		const first = await f.controller.projectRequest(new Set(["request"]), near);
		expect(f.controller.getClmStatus().warning).toBeTruthy();
		expect(await f.controller.projectRequest(new Set(["request"]), near)).toEqual(first);
		await expect(
			f.controller.projectRequest(new Set(["request"]), { ...near, systemPrompt: "x".repeat(4000) }),
		).rejects.toThrow(/budget/i);
		expect(f.controller.getClmStatus().revision).toBe(0);
		await f.project();
		expect(f.controller.getClmStatus().warning).toBeUndefined();
	});

	it.each(["external", "bom"])("fails closed on %s pre-action drift and refuses later projection", async (kind) => {
		const f = await fixture();
		await writeFile(f.path, kind === "bom" ? `\uFEFF${await readFile(f.path, "utf8")}` : "external bytes");
		await expect(f.prepare()).rejects.toThrow(/drift|conflict/i);
		await expect(f.project()).rejects.toThrow(/drift|conflict/i);
		expect(f.controller.getClmStatus().revision).toBe(0);
	});

	it("pins action ownership across queued prepare and mismatched settlement", async () => {
		const f = await fixture();
		const results = await Promise.allSettled([f.prepare(), f.prepare()]);
		expect(results.map((result) => result.status)).toEqual(["fulfilled", "rejected"]);
		await expect(f.project()).rejects.toThrow(/action|writer/i);
		await expect(f.controller.settleToolAction("other", f.protectedActions, budget)).rejects.toThrow(/action/i);
		await expect(f.controller.settleToolAction("tool", new Map(), budget)).rejects.toThrow(/protect|action/i);
		expect(f.controller.getClmStatus().revision).toBe(0);
	});

	it.each(["unknown", "payload", "missing-budget"])("fails closed for %s request inputs", async (kind) => {
		const f = await fixture();
		const request =
			kind === "payload"
				? { ...f.request, protectedActions: new Map([["request", [{ ...f.blocks[1], message: user("changed") }]]]) }
				: f.request;
		if (kind === "missing-budget") Reflect.deleteProperty(request, "model");
		await expect(
			f.controller.projectRequest(new Set([kind === "unknown" ? "unknown" : "request"]), request),
		).rejects.toThrow();
	});

	it("includes uncovered audit messages exactly once after committing an edit", async () => {
		const f = await fixture();
		const next = user("new delivered action");
		const id = f.manager.appendMessage(next);
		f.protectedActions.set("next", [{ id, message: next }]);
		await f.prepare();
		await f.change((text) => text.replace("obsolete", "summary"));
		await f.settle();
		const projection = await f.project();
		expect(projection.filter((message) => JSON.stringify(message) === JSON.stringify(next))).toHaveLength(1);
		expect(f.manager.getClmBranchState().auditWatermark).toBe(id);
	});

	it("durably removes only the exact failed provider entry and reopens its accepted revision", async () => {
		const f = await fixture();
		await f.prepare();
		await f.change((text) => text.replace("obsolete", "edited note"));
		await f.settle();
		const message = fauxAssistantMessage(fauxToolCall("inspect", {}, { id: "partial" }), { stopReason: "error" });
		const id = f.manager.appendMessage(message);
		f.manager.appendModelChange(model.provider, model.id);
		await f.controller.discardFailedProviderEntry({ id, message }, f.protectedActions);
		const state = f.manager.getClmBranchState();
		expect(state).toMatchObject({ revision: 2, auditWatermark: id });
		expect(f.manager.getEntry(id)).toMatchObject({ message });
		expect(f.manager.getBranch().at(-1)).toMatchObject({ observedActionId: `failed-provider:${id}` });
		const reopened = await LiveContextController.open(
			SessionManager.open(f.manager.getSessionFile()!),
			join(f.dir, "reopened"),
		);
		const projected = JSON.stringify(await reopened.projectRequest(new Set(["request"]), f.request));
		expect(projected).toContain("edited note");
		expect(projected).toContain("current request");
		expect(projected).not.toContain("partial");
	});
	it("fails closed on an uncovered failed provider entry without ownership proof, including reopen", async () => {
		const f = await fixture();
		f.manager.appendMessage(fauxAssistantMessage(fauxToolCall("inspect", {}), { stopReason: "error" }));
		await expect(f.project()).rejects.toThrow(/failed provider.*cleanup/i);
		await expect(f.prepare()).rejects.toThrow(/failed provider.*cleanup/i);
		const reopened = SessionManager.open(f.manager.getSessionFile()!);
		await expect(LiveContextController.open(reopened, join(f.dir, "reopened"))).rejects.toThrow(
			/failed provider.*cleanup/i,
		);
	});
});
