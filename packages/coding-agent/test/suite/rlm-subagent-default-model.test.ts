import { writeFileSync } from "node:fs";
import { fauxAssistantMessage, fauxToolCall } from "@earendil-works/pi-ai";
import { describe, expect, it, vi } from "vitest";
import { createHarness } from "./harness.js";
import { createDeferred } from "./scheduling.js";

const provider = "faux-eng-subagent-default-model";

describe("subagent default model setting", () => {
	it("gives an inline spawned child an independent CLM file and extension context", async () => {
		const contexts: string[] = [];
		const payloads: string[] = [];
		const responses: string[] = [];
		const h = await createHarness({
			persistSession: true,
			extensionFactories: [
				(pi) => {
					pi.on("before_provider_request", (_event, ctx) => {
						payloads.push(ctx.sessionManager.getSessionId());
					});
					pi.on("after_provider_response", (_event, ctx) => {
						responses.push(ctx.sessionManager.getSessionId());
					});
					pi.on("context", (_event, ctx) => {
						contexts.push(ctx.sessionManager.getSessionId());
					});
				},
			],
		});
		try {
			h.setResponses([fauxAssistantMessage("parent answer")]);
			await h.session.prompt("obsolete parent-only content");
			const parent = await h.session.setClmMode(true);
			const requests: string[] = [];
			h.setResponses([
				async (context, options, _state, model) => {
					await options?.onPayload?.({}, model);
					requests.push(JSON.stringify(context.messages));
					return fauxAssistantMessage("child answer");
				},
			]);
			const spawned = await h.session.runRlmChild("independent child task");
			await h.session.collectRlmChildren([spawned.rlm_child_id], 10_000);
			const child = h.session.getRlmChildSession(spawned.rlm_child_id)!;
			const status = await child.getClmStatus();
			expect(status.mode).toBe("on");
			expect(status.path).not.toBe(parent.path);
			expect(status.epochId).not.toBe(parent.epochId);
			expect(requests).toHaveLength(1);
			expect(requests[0]).toContain("independent child task");
			expect(requests[0]).not.toContain("obsolete parent-only content");
			expect(contexts.at(-1)).toBe(child.sessionId);
			expect(payloads.at(-1)).toBe(child.sessionId);
			expect(responses.at(-1)).toBe(child.sessionId);
			await h.session.waitForSessionInputIdle();
			await h.session.restoreClmRevision(parent.epochId!, 0);
			expect((await child.getClmStatus()).revision).toBe(status.revision);
			await h.session.setClmMode(false);
			expect((await child.getClmStatus()).mode).toBe("on");
			h.settingsManager.applyOverrides({ clm: { enabled: true } });
			h.setResponses([fauxAssistantMessage("off child")]);
			const next = await h.session.runRlmChild("off child task");
			await h.session.collectRlmChildren([next.rlm_child_id], 10_000);
			expect((await h.session.getRlmChildSession(next.rlm_child_id)!.getClmStatus()).mode).toBe("off");
			const resumed = await createHarness({
				existingSessionFile: child.sessionFile!,
				settings: { clm: { enabled: false } },
			});
			try {
				expect((await resumed.session.getClmStatus()).epochId).toBe(status.epochId);
			} finally {
				await resumed.session.disposeAsync();
				resumed.cleanup();
			}
		} finally {
			await h.session.disposeAsync();
			h.cleanup();
		}
	});

	it("admits rlm.spawn from its owned live ipython writer", async () => {
		const h = await createHarness({ persistSession: true });
		try {
			await h.session.setClmMode(true);
			h.session.setActiveToolsByName(["ipython"]);
			h.setResponses([
				fauxAssistantMessage(
					fauxToolCall("ipython", { code: 'child = await rlm.spawn("kernel child task", name="kernel-child")' }),
					{ stopReason: "toolUse" },
				),
				...Array.from({ length: 3 }, () => fauxAssistantMessage("finished")),
			]);
			await h.session.prompt("spawn through kernel");
			const collected = await h.session.collectRlmChildren([], 10_000);
			expect(collected.results).toHaveLength(1);
			expect(collected.results[0].status).toBe("done");
			const child = h.session.getRlmChildSession(collected.results[0].rlm_child_id)!;
			expect((await child.getClmStatus()).mode).toBe("on");
			expect(JSON.stringify(await child.getAcceptedModelContextSnapshot())).toContain("kernel child task");
		} finally {
			await h.session.disposeAsync();
			h.cleanup();
		}
	});

	it.each(["toggle", "marker"])("pins admission mode across asynchronous auth with %s", async (change) => {
		const h = await createHarness({ persistSession: true });
		const started = createDeferred();
		const release = createDeferred();
		const auth = h.session.modelRegistry.getApiKeyAndHeaders.bind(h.session.modelRegistry);
		const held = vi.spyOn(h.session.modelRegistry, "getApiKeyAndHeaders").mockImplementationOnce(async (model) => {
			started.resolve();
			await release.promise;
			return auth(model);
		});
		try {
			await h.session.setClmMode(true);
			h.setResponses([fauxAssistantMessage("child")]);
			const spawn = h.session.runRlmChild("pinned task", { model: `${h.getModel().provider}/${h.getModel().id}` });
			await started.promise;
			if (change === "toggle") await h.session.setClmMode(false);
			else writeFileSync(`${h.session.sessionFile}.clm-writer`, "unknown");
			release.resolve();
			if (change === "marker") {
				await expect(spawn).rejects.toThrow(/CLM/);
				expect(h.faux.state.callCount).toBe(0);
				expect((await h.session.listRlmSubagents()).subagents).toEqual([]);
			} else {
				const child = await spawn;
				await h.session.collectRlmChildren([child.rlm_child_id], 10_000);
				expect((await h.session.getRlmChildSession(child.rlm_child_id)!.getClmStatus()).mode).toBe("on");
			}
		} finally {
			release.resolve();
			held.mockRestore();
			await h.session.disposeAsync();
			h.cleanup();
		}
	});

	it("resolves unpinned spawns against subagentDefaultModel", { timeout: 30_000 }, async () => {
		const harness = await createHarness({
			provider,
			models: [{ id: "parent-model" }, { id: "child-model" }],
			settings: { subagentDefaultModel: `${provider}/child-model` },
		});
		try {
			harness.setResponses([fauxAssistantMessage("child answer")]);

			const result = await harness.session.runRlmChild("do the work");

			expect(result.model).toBe(`${provider}/child-model`);
			await vi.waitFor(
				async () => {
					const childEntry = (await harness.session.listRlmSubagents()).subagents[0];
					expect(childEntry?.status).toBe("completed");
					expect(harness.session.getRlmChildSession(childEntry!.rlm_child_id)?.model?.id).toBe("child-model");
				},
				{ timeout: 10_000 },
			);
		} finally {
			harness.cleanup();
		}
	});

	it("keeps an explicit spawn model ahead of subagentDefaultModel", { timeout: 30_000 }, async () => {
		const harness = await createHarness({
			provider,
			models: [{ id: "parent-model" }, { id: "child-model" }],
			settings: { subagentDefaultModel: `${provider}/child-model` },
		});
		try {
			harness.setResponses([fauxAssistantMessage("parent answer")]);

			const result = await harness.session.runRlmChild("do the work", { model: `${provider}/parent-model` });

			expect(result.model).toBe(`${provider}/parent-model`);
			await vi.waitFor(
				async () => {
					const childEntry = (await harness.session.listRlmSubagents()).subagents[0];
					expect(childEntry?.status).toBe("completed");
					expect(harness.session.getRlmChildSession(childEntry!.rlm_child_id)?.model?.id).toBe("parent-model");
				},
				{ timeout: 10_000 },
			);
		} finally {
			harness.cleanup();
		}
	});

	it("fails an unpinned spawn when the configured default is unavailable", { timeout: 30_000 }, async () => {
		const harness = await createHarness({
			provider,
			models: [{ id: "parent-model" }],
			settings: { subagentDefaultModel: `${provider}/missing-model` },
		});
		try {
			await expect(harness.session.runRlmChild("do the work")).rejects.toThrow(
				`Requested subagent model "${provider}/missing-model" is unavailable, unauthenticated, or expired`,
			);
			expect((await harness.session.listRlmSubagents()).subagents).toEqual([]);
		} finally {
			harness.cleanup();
		}
	});

	it("fails an unpinned spawn when the configured parent-model default is stale", { timeout: 30_000 }, async () => {
		// A default naming the parent model must pass the same availability and
		// authentication preflight as any other reference; the parent-model
		// equality shortcut would start a child that fails its model request.
		const harness = await createHarness({
			provider,
			models: [{ id: "parent-model" }, { id: "child-model" }],
			settings: { subagentDefaultModel: `${provider}/parent-model` },
		});
		try {
			expect(harness.session.modelRegistry.markProviderAuthStale(provider)).toBe(true);
			expect(harness.session.modelRegistry.markProviderAuthStale(provider)).toBe(true);
			expect(harness.session.modelRegistry.getProviderAuthStatus(provider)).toMatchObject({
				source: "stale",
				label: "expired",
			});
			harness.setResponses([fauxAssistantMessage("child answer")]);

			await expect(harness.session.runRlmChild("do the work")).rejects.toThrow(
				`Requested subagent model "${provider}/parent-model" is unavailable, unauthenticated, or expired`,
			);
			expect((await harness.session.listRlmSubagents()).subagents).toEqual([]);
		} finally {
			harness.cleanup();
		}
	});

	it("inherits the parent model when no default is configured", { timeout: 30_000 }, async () => {
		const harness = await createHarness({
			provider,
			models: [{ id: "parent-model" }, { id: "child-model" }],
		});
		try {
			harness.setResponses([fauxAssistantMessage("parent answer")]);

			const result = await harness.session.runRlmChild("do the work");

			expect(result.model).toBe(`${provider}/parent-model`);
			await vi.waitFor(
				async () => {
					const childEntry = (await harness.session.listRlmSubagents()).subagents[0];
					expect(childEntry?.status).toBe("completed");
				},
				{ timeout: 10_000 },
			);
		} finally {
			harness.cleanup();
		}
	});

	it("treats a non-string subagentDefaultModel setting as unset on unpinned spawns", { timeout: 30_000 }, async () => {
		// Corrupted settings (e.g. 42) must degrade to "unset" in the spawn hot
		// path: the child inherits the parent model instead of throwing.
		const harness = await createHarness({
			provider,
			models: [{ id: "parent-model" }, { id: "child-model" }],
			settings: { subagentDefaultModel: 42 as unknown as string },
		});
		try {
			harness.setResponses([fauxAssistantMessage("parent answer")]);

			const result = await harness.session.runRlmChild("do the work");

			expect(result.model).toBe(`${provider}/parent-model`);
			await vi.waitFor(
				async () => {
					const childEntry = (await harness.session.listRlmSubagents()).subagents[0];
					expect(childEntry?.status).toBe("completed");
				},
				{ timeout: 10_000 },
			);
		} finally {
			harness.cleanup();
		}
	});
});
