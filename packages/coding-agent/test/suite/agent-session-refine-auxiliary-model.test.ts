import { writeFileSync } from "node:fs";
import type * as PiAi from "@earendil-works/pi-ai";
import type { AssistantMessage } from "@earendil-works/pi-ai";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { userMsg } from "../utilities.js";
import { createDeferred } from "./scheduling.js";

const { completeSimpleMock, original } = vi.hoisted(() => ({
	completeSimpleMock: vi.fn(),
	original: {} as { completeSimple: typeof PiAi.completeSimple },
}));

vi.mock("@earendil-works/pi-ai", async (importOriginal) => {
	const actual = await importOriginal<typeof PiAi>();
	original.completeSimple = actual.completeSimple;
	return {
		...actual,
		completeSimple: completeSimpleMock,
	};
});

import { createHarness, type Harness } from "./harness.js";

function assistantText(text: string): AssistantMessage {
	return {
		role: "assistant",
		content: [{ type: "text", text }],
		api: "faux",
		provider: "faux",
		model: "faux",
		usage: {
			input: 1,
			output: 1,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: 2,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		},
		stopReason: "stop",
		timestamp: Date.now(),
	};
}

function emptyProposalResponse(): AssistantMessage {
	return assistantText(
		JSON.stringify({ summary: "no-op", rationale: "nothing to record", expectedOutcome: "none", edits: [] }),
	);
}

describe("AgentSession refinement auxiliary model", () => {
	const harnesses: Harness[] = [];

	beforeEach(() => {
		completeSimpleMock.mockReset();
		completeSimpleMock.mockResolvedValue(emptyProposalResponse());
	});

	afterEach(() => {
		for (const harness of harnesses.splice(0)) harness.cleanup();
	});

	async function createRefineHarness(settings?: { auxiliaryModel?: string }): Promise<Harness> {
		const harness = await createHarness({
			models: [
				{ id: "session-model", name: "Session Model" },
				{ id: "aux-model", name: "Aux Model" },
			],
			settings,
			persistSession: true,
		});
		harnesses.push(harness);
		return harness;
	}

	it.each(["on", "off", "plain"])("lets %s refinement completion hooks read accepted context", async (mode) => {
		const completed = createDeferred();
		const release = createDeferred();
		const order: string[] = [];
		let observed = "";
		let persisted = false;
		let timer: ReturnType<typeof setTimeout> | undefined;
		const h = await createHarness({
			persistSession: true,
			extensionFactories: [
				(pi) => {
					pi.on("refine_complete", async (event) => {
						await Promise.race([
							h.session.getAcceptedModelContextSnapshot().then((messages) => {
								observed = JSON.stringify(messages);
								persisted = JSON.stringify(h.sessionManager.getEntries()).includes(event.id);
								order.push("extension");
								completed.resolve();
							}),
							release.promise,
						]);
						throw new Error("completion listener failed after persistence");
					});
				},
			],
		});
		const unsubscribe = h.session.subscribe((event) => {
			if (event.type === "refine_complete") order.push("session");
		});
		let refinement: ReturnType<typeof h.session.refine> | undefined;
		try {
			await h.session.prompt("accepted hook context");
			if (mode !== "plain") await h.session.setClmMode(true);
			if (mode === "off") await h.session.setClmMode(false);
			completeSimpleMock.mockImplementation(original.completeSimple);
			h.setResponses([emptyProposalResponse()]);
			refinement = h.session.refine();
			const timeout = new Promise<boolean>((resolve) => {
				// test-policy: allow wall-clock-timer -- Bounds a deadlocked completion hook; only the deferred snapshot can pass.
				timer = setTimeout(() => resolve(false), 2000);
			});
			expect(await Promise.race([completed.promise.then(() => true), timeout])).toBe(true);
			await refinement;
			order.push("returned");
			expect(observed).toContain("accepted hook context");
			expect(persisted).toBe(true);
			expect(order).toEqual(["session", "extension", "returned"]);
		} finally {
			clearTimeout(timer);
			release.resolve();
			await refinement;
			unsubscribe();
			await h.session.disposeAsync();
			h.cleanup();
		}
	});

	it.each(["manual", "automatic", "off", "marker", "apply-marker"])(
		"pins accepted-only context for %s refinement",
		async (mode) => {
			const preparations: string[] = [];
			const h = await createHarness({
				persistSession: true,
				serializedRefine: mode === "automatic",
				settings: { autoRefine: { enabled: mode === "automatic", turnInterval: 1, cooldownMs: 0 } },
				extensionFactories: [
					(pi) => {
						pi.on("session_before_refine", (event) => {
							preparations.push(event.preparation.conversationText);
						});
					},
				],
			});
			harnesses.push(h);
			const watermark = h.sessionManager.appendMessage(userMsg("obsolete audit"));
			h.session.agent.state.messages = h.sessionManager.buildSessionContext().messages;
			h.sessionManager.appendClmOn({ blocks: [{ id: watermark, message: userMsg("obsolete audit") }] }, watermark);
			h.sessionManager.appendClmRevision(
				{ blocks: [{ id: watermark, message: userMsg("accepted context") }] },
				watermark,
				"forget",
			);
			completeSimpleMock.mockImplementation(original.completeSimple);
			const requests: string[] = [];
			h.setResponses([
				...(mode === "automatic"
					? [
							assistantText("turn"),
							(context: PiAi.Context) => {
								requests.push(JSON.stringify(context.messages));
								return assistantText(JSON.stringify({ shouldRefine: true, rationale: "record learning" }));
							},
						]
					: []),
				(context) => {
					requests.push(JSON.stringify(context.messages));
					if (mode === "apply-marker") writeFileSync(`${h.session.sessionFile}.clm-writer`, "unknown");
					return emptyProposalResponse();
				},
			]);
			if (mode === "off") await h.session.setClmMode(false);
			if (mode === "marker") {
				writeFileSync(`${h.session.sessionFile}.clm-writer`, "unknown");
				await expect(h.session.refine()).rejects.toThrow(/CLM/);
				expect(h.faux.state.callCount).toBe(0);
				expect(preparations).toEqual([]);
			} else if (mode === "apply-marker") {
				await expect(h.session.refine()).rejects.toThrow(/CLM/);
				expect(h.sessionManager.getEntries().filter((entry) => entry.type === "custom_message")).toEqual([]);
			} else {
				if (mode === "automatic") await h.session.prompt("next");
				else await h.session.refine();
				expect(requests).toHaveLength(mode === "automatic" ? 2 : 1);
			}
			for (const text of [...preparations, ...requests]) {
				expect(text).toContain("accepted context");
				expect(text).not.toContain("obsolete audit");
			}
			expect(JSON.stringify(h.sessionManager.getEntries())).toContain("obsolete audit");
			if (!mode.endsWith("marker")) {
				const pinned = await h.session.getAcceptedModelContextSnapshot();
				pinned.length = 0;
				expect(JSON.stringify(await h.session.getAcceptedModelContextSnapshot())).toContain("accepted context");
			}
		},
	);

	it("keeps the original accepted revision on a refinement retry after a new revision appears", async () => {
		const h = await createHarness({
			persistSession: true,
			settings: { retry: { enabled: true, maxRetries: 1, baseDelayMs: 0 } },
		});
		harnesses.push(h);
		const watermark = h.sessionManager.appendMessage(userMsg("task"));
		h.sessionManager.appendClmOn({ blocks: [{ id: watermark, message: userMsg("task") }] }, watermark);
		const epochId = h.sessionManager.getClmBranchState().epochId;
		const first = { ...assistantText("transient"), stopReason: "error" as const };
		first.usage.cacheRead = 9;
		const second = emptyProposalResponse();
		second.usage.cacheRead = 20;
		completeSimpleMock
			.mockImplementationOnce(async () => {
				h.sessionManager.appendClmRevision(
					{ blocks: [{ id: watermark, message: userMsg("changed") }] },
					watermark,
					"restore:simulated",
				);
				return first;
			})
			.mockResolvedValueOnce(second);
		await expect(h.session.refine()).rejects.toThrow(/CLM accepted state changed/);
		const receipts = h.sessionManager
			.getBranch()
			.filter((entry) => entry.type === "custom" && entry.customType === "clm_usage_v1");
		expect(receipts.map((entry) => (entry.type === "custom" ? entry.data : null))).toMatchObject([
			{ pin: { epochId, revision: 0 }, usage: { cacheRead: 9 } },
			{ pin: { epochId, revision: 0 }, usage: { cacheRead: 20 } },
		]);
		expect(h.sessionManager.getClmMeasuredUsage()).toBeUndefined();
	});

	it("routes refinement planning to the configured auxiliary model", async () => {
		const harness = await createRefineHarness({ auxiliaryModel: "faux/aux-model" });
		await harness.session.refine({ instructions: "record a note" });
		expect(completeSimpleMock).toHaveBeenCalledTimes(1);
		expect(completeSimpleMock.mock.calls[0][0]).toMatchObject({
			provider: "faux",
			id: "aux-model",
		});
	});

	it("falls back to the session model when no auxiliary model is configured", async () => {
		const harness = await createRefineHarness();
		await harness.session.refine({ instructions: "record a note" });
		expect(completeSimpleMock).toHaveBeenCalledTimes(1);
		expect(completeSimpleMock.mock.calls[0][0]).toMatchObject({
			provider: "faux",
			id: "session-model",
		});
	});

	it("falls back to the session model when the auxiliary model is unusable", async () => {
		const warnSpy = vi.spyOn(console, "warn").mockImplementation(() => {});
		try {
			const harness = await createRefineHarness({ auxiliaryModel: "faux/missing-model" });
			await harness.session.refine({ instructions: "record a note" });
			expect(completeSimpleMock).toHaveBeenCalledTimes(1);
			expect(completeSimpleMock.mock.calls[0][0]).toMatchObject({
				provider: "faux",
				id: "session-model",
			});
			expect(warnSpy).toHaveBeenCalledTimes(1);
			const [message] = warnSpy.mock.calls[0];
			expect(message).toContain('auxiliaryModel "faux/missing-model" unusable for refinement');
			// Caught error details can embed credential material, so they must not be logged.
			expect(message).not.toContain("unavailable, unauthenticated, or expired");
		} finally {
			warnSpy.mockRestore();
		}
	});
});
