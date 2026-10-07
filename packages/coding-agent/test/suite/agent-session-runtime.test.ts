import { existsSync, mkdirSync, readdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fauxAssistantMessage, fauxToolCall, registerFauxProvider } from "@earendil-works/pi-ai";
import { Type } from "typebox";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AgentSession } from "../../src/core/agent-session.js";
import type { AgentSessionRuntimeConfig } from "../../src/core/agent-session-config.js";
import {
	AgentSessionRuntime,
	type AgentSessionServices,
	type CreateAgentSessionRuntimeFactory,
	createAgentSessionFromServices,
	createAgentSessionRuntime,
	createAgentSessionServices,
} from "../../src/core/agent-session-runtime.js";
import { AuthStorage } from "../../src/core/auth-storage.js";
import { renderContextDocument } from "../../src/core/clm/context-document.js";
import { LiveContextController } from "../../src/core/clm/live-context.js";
import type { SubagentRuntimeHost } from "../../src/core/rlm-runtime.js";
import { createAgentSession } from "../../src/core/sdk.js";
import {
	deriveSemanticEdges,
	readSemanticEdgeLedger,
	SEMANTIC_EDGES_LEDGER_FILENAME,
} from "../../src/core/semantic-edges.js";
import { SESSION_LEASE_OWNER_ID_ENV, SESSION_LEASES_ENABLED_ENV } from "../../src/core/session-lease.js";
import { SessionManager } from "../../src/core/session-manager.js";
import { SettingsManager } from "../../src/core/settings-manager.js";
import type {
	ExtensionAPI,
	ExtensionFactory,
	SessionBeforeForkEvent,
	SessionBeforeSwitchEvent,
	SessionShutdownEvent,
	SessionStartEvent,
} from "../../src/index.js";
import { createDefaultRuntimeFactory } from "../../src/main.js";
import { InProcessAgentConnection } from "../../src/modes/agent-connection/in-process-agent-connection.js";
import { assistantMsg, createTestResourceLoader, userMsg } from "../utilities.js";
import { conversationMessages, createHarness, getMessageText, type Harness } from "./harness.js";

import { createDeferred } from "./scheduling.js";

type RecordedSessionEvent =
	| SessionBeforeSwitchEvent
	| SessionBeforeForkEvent
	| SessionShutdownEvent
	| SessionStartEvent;

type RuntimeSubagentMapAccess = {
	subagentRuntimes: Map<string, AgentSessionRuntime>;
};

describe("AgentSessionRuntime characterization", () => {
	const cleanups: Array<() => Promise<void> | void> = [];

	afterEach(async () => {
		while (cleanups.length > 0) {
			await cleanups.pop()?.();
		}
		vi.unstubAllEnvs();
	});

	async function createRuntimeForTest(
		extensionFactory: ExtensionFactory,
		options?: {
			cwd?: string;
			bootstrapModel?: boolean;
			bootstrapThinkingLevel?: boolean;
			inMemory?: boolean;
			sessionConfig?: AgentSessionRuntimeConfig;
			sessionManager?: SessionManager;
			sessionOptions?: Parameters<CreateAgentSessionRuntimeFactory>[0]["sessionOptions"];
			onCreateRuntime?: (options: Parameters<CreateAgentSessionRuntimeFactory>[0]) => void;
		},
	) {
		const tempDir =
			options?.cwd ?? join(tmpdir(), `pi-runtime-suite-${Date.now()}-${Math.random().toString(36).slice(2)}`);
		mkdirSync(tempDir, { recursive: true });

		const faux = registerFauxProvider({
			models: [
				{ id: "faux-1", reasoning: true },
				{ id: "faux-2", reasoning: false },
			],
		});
		faux.setResponses([fauxAssistantMessage("one"), fauxAssistantMessage("two"), fauxAssistantMessage("three")]);

		const authStorage = AuthStorage.inMemory();
		authStorage.setRuntimeApiKey(faux.getModel().provider, "faux-key");

		const serviceOptions = {
			agentDir: tempDir,
			authStorage,
			model: options?.bootstrapModel === false ? undefined : faux.getModel(),
			thinkingLevel: options?.bootstrapThinkingLevel === false ? undefined : undefined,
			resourceLoaderOptions: {
				extensionFactories: [
					(pi: ExtensionAPI) => {
						pi.registerProvider(faux.getModel().provider, {
							baseUrl: faux.getModel().baseUrl,
							apiKey: "faux-key",
							api: faux.api,
							models: faux.models.map((registeredModel) => ({
								id: registeredModel.id,
								name: registeredModel.name,
								api: registeredModel.api,
								reasoning: registeredModel.reasoning,
								input: registeredModel.input,
								cost: registeredModel.cost,
								contextWindow: registeredModel.contextWindow,
								maxTokens: registeredModel.maxTokens,
							})),
						});
						extensionFactory(pi);
					},
				],
				noSkills: true,
				noPromptTemplates: true,
				noThemes: true,
			},
		};
		const createRuntime: CreateAgentSessionRuntimeFactory = async (runtimeOptions) => {
			options?.onCreateRuntime?.(runtimeOptions);
			const { cwd, sessionManager, sessionStartEvent } = runtimeOptions;
			const services = await createAgentSessionServices({
				...serviceOptions,
				cwd,
			});
			return {
				...(await createAgentSessionFromServices({
					services,
					sessionManager,
					sessionStartEvent,
					model: serviceOptions.model,
					thinkingLevel: serviceOptions.thinkingLevel,
					...runtimeOptions.sessionOptions,
				})),
				services,
				diagnostics: services.diagnostics,
			};
		};
		const runtime = await createAgentSessionRuntime(createRuntime, {
			cwd: tempDir,
			agentDir: tempDir,
			sessionManager:
				options?.sessionManager ??
				(options?.inMemory
					? SessionManager.inMemory(tempDir)
					: SessionManager.create(tempDir, join(tempDir, "sessions"))),
			sessionConfig: options?.sessionConfig,
			sessionOptions: options?.sessionOptions,
		});
		await runtime.session.bindExtensions({});

		cleanups.push(async () => {
			await runtime.dispose();
			faux.unregister();
			if (existsSync(tempDir)) {
				rmSync(tempDir, { recursive: true, force: true });
			}
		});

		return { runtime, faux, tempDir };
	}

	function createRuntimeWithFakeSession(options?: { onShutdown?: () => void }) {
		const disposeSession = vi.fn();
		const session = {
			extensionRunner: {
				hasHandlers: (event: string) => event === "session_shutdown" && options?.onShutdown !== undefined,
				emit: async () => {
					options?.onShutdown?.();
				},
			},
			setSubagentRuntimeHost: vi.fn(),
			dispose: disposeSession,
			disposeAsync: disposeSession,
		} as unknown as AgentSession;
		const services = { cwd: "/tmp", agentDir: "/tmp" } as unknown as AgentSessionServices;
		const createRuntime: CreateAgentSessionRuntimeFactory = async () => {
			throw new Error("unexpected runtime creation");
		};
		const runtime = new AgentSessionRuntime(session, services, createRuntime);
		return { runtime, disposeSession };
	}

	it.each(["corrupt", "malformed-on", "on", "off"] as const)("attaches %s CLM without audit replay", async (mode) => {
		const harness = await createHarness({ persistSession: true });
		cleanups.push(harness.cleanup);
		const manager = harness.sessionManager;
		const watermark =
			mode === "malformed-on"
				? manager.appendThinkingLevelChange("off")
				: manager.appendMessage(userMsg("original audit only"));
		const accepted = { blocks: [{ id: "accepted", message: userMsg("accepted context") }] };
		if (mode === "malformed-on") {
			manager.flushNow();
		} else {
			manager.appendClmOn(accepted, watermark);
			manager.appendClmRevision(accepted, watermark, "settled-action");
			if (mode === "off") manager.appendClmOffBoundary(accepted, watermark);
		}
		const path = manager.getSessionFile()!;
		if (mode === "corrupt") {
			writeFileSync(path, readFileSync(path, "utf8").replace(/"checksum":"[^"]+"/, '"checksum":"damaged"'));
		}
		if (mode === "malformed-on") writeFileSync(path, `${readFileSync(path, "utf8")}{"type":"clm_on",\n`);
		const before = readFileSync(path, "utf8");
		const startup = createRuntimeForTest(() => {}, {
			sessionManager: SessionManager.open(path),
			sessionOptions: { initialGoal: { objective: "must not seed" } },
		});
		await expect(startup).resolves.toHaveProperty("runtime");
		const { runtime, faux } = await startup;
		const connection = new InProcessAgentConnection(runtime);
		cleanups.push(() => connection.dispose());
		const requests: string[] = [];
		faux.setResponses([
			(context) => {
				requests.push(JSON.stringify(context.messages));
				return fauxAssistantMessage("reply");
			},
		]);
		const snapshot = await connection.getInitialSnapshot();
		if (mode === "corrupt" || mode === "malformed-on") {
			expect(readFileSync(path, "utf8")).toBe(before);
			expect(runtime.session.goalState.status).toBe("idle");
			expect(() => SessionManager.open(path).buildSessionContext()).toThrow();
			const audit = mode === "malformed-on" ? [] : ["original audit only"];
			expect(snapshot.messages.map(getMessageText)).toEqual(audit);
			expect((await connection.getMessages()).map(getMessageText)).toEqual(audit);
			expect(snapshot.sessionTree?.tree.length).toBeGreaterThan(0);
			expect(snapshot.sessionContext).toBeUndefined();
			expect(runtime.session.agent.state.messages).toEqual([]);
			await expect(connection.getSessionContext()).rejects.toMatchObject({ name: "ClmRecoveryError" });
			for (const request of [
				() => connection.prompt("new request"),
				() => connection.compact(),
				() => connection.refine(),
				() => connection.startSideQuestion("side", "question"),
				() => runtime.session.runRlmChild("child"),
				() => runtime.session.navigateTree(watermark, { summarize: true }),
			])
				await expect(request()).rejects.toMatchObject({ name: "ClmRecoveryError" });
			expect(requests).toEqual([]);
			expect(readFileSync(path, "utf8")).toBe(before);
			runtime.session.setAutoRetryEnabled(false);
			await runtime.session.agent.prompt("direct low-level continuation");
			expect(requests).toEqual([]);
		} else {
			await connection.promptAndWait("new request");
			expect(requests).toHaveLength(1);
			expect(requests[0]).toContain("accepted context");
			expect(requests[0]).not.toContain("original audit only");
		}
	});

	it("keeps forgotten audit text out after CLM turns off and opens a fresh epoch", async () => {
		const { runtime, faux } = await createRuntimeForTest(() => {});
		const session = runtime.session;
		await session.prompt("obsolete");
		const on = await session.setClmMode(true);
		const manager = session.sessionManager;
		const controller = await LiveContextController.open(manager, manager.getSessionArtifactDir()!);
		await controller.prepareToolAction("forget", new Map());
		const snapshot = manager.getClmBranchState().snapshot!;
		snapshot.blocks = snapshot.blocks.filter((block) => !JSON.stringify(block).includes("obsolete"));
		writeFileSync(on.path, renderContextDocument(snapshot, new Set()));
		await controller.settleToolAction("forget", new Map(), { model: faux.getModel(), tools: [], systemPrompt: "" });
		await session.setClmMode(false);
		expect(JSON.stringify(manager.buildSessionContext().messages)).not.toContain("obsolete");
		faux.setResponses([
			(context) => {
				expect(JSON.stringify(context.messages)).not.toContain("obsolete");
				return fauxAssistantMessage("after off");
			},
		]);
		await session.prompt("continue");
		const reopened = await session.setClmMode(true);
		expect(reopened.epochId).not.toBe(on.epochId);
		expect(reopened.revision).toBe(0);
		expect(readFileSync(reopened.path, "utf8")).not.toContain("obsolete");
		await session.restoreClmRevision(reopened.epochId!, 0);
		expect((await session.getClmStatus()).revision).toBe(1);
		await expect(session.restoreClmRevision(on.epochId!, 0)).rejects.toThrow(/CLM/);
	});

	it("uses defaults only for new sessions and persists explicit mode across reload and resume", async () => {
		const h = await createHarness({ persistSession: true, settings: { clm: { enabled: true } } });
		cleanups.push(h.cleanup);
		expect(h.sessionManager.getClmBranchState().mode).toBe("on");
		h.settingsManager.applyOverrides({ clm: { enabled: false } });
		await h.settingsManager.reload();
		expect((await h.session.getClmStatus()).mode).toBe("on");
		const path = h.session.sessionFile!;
		await h.session.disposeAsync();
		const { runtime, tempDir } = await createRuntimeForTest(() => {}, { sessionManager: SessionManager.open(path) });
		expect((await runtime.session.getClmStatus()).mode).toBe("on");
		await runtime.session.setClmMode(false);
		writeFileSync(join(tempDir, "settings.json"), JSON.stringify({ clm: { enabled: true } }));
		await runtime.services.settingsManager.reload();
		expect((await runtime.session.getClmStatus()).mode).toBe("off");
		await runtime.switchSession(path);
		expect((await runtime.session.getClmStatus()).mode).toBe("off");
		await runtime.newSession();
		expect((await runtime.session.getClmStatus()).mode).toBe("on");
	});

	it("restores only active-epoch ancestor revisions with the current watermark", async () => {
		const { runtime } = await createRuntimeForTest(() => {});
		const s = runtime.session;
		await s.prompt("first");
		const on = await s.setClmMode(true);
		await s.prompt("later");
		const watermark = s.sessionManager
			.getBranch()
			.filter((e) => e.type === "message")
			.at(-1)!.id;
		await s.restoreClmRevision(on.epochId!, 0);
		expect(s.sessionManager.getClmBranchState()).toMatchObject({ revision: 1, auditWatermark: watermark });
		expect(JSON.stringify(s.buildSessionContext().messages)).not.toContain("later");
		const ancestor = s.sessionManager.getLeafId()!;
		s.sessionManager.appendClmRevision({ blocks: [] }, watermark, "abandoned");
		s.sessionManager.branch(ancestor);
		const before = readFileSync(s.sessionFile!, "utf8");
		await expect(s.restoreClmRevision(on.epochId!, 2)).rejects.toThrow(/CLM/);
		expect(readFileSync(s.sessionFile!, "utf8")).toBe(before);
		await s.setClmMode(false);
		await expect(s.restoreClmRevision(on.epochId!, 0)).rejects.toThrow(/CLM/);
	});

	it("rejects mode changes and same-path switching before opening an active or cancelled CLM writer", async () => {
		const started = createDeferred();
		const release = createDeferred();
		const settled = createDeferred();
		const { runtime, faux } = await createRuntimeForTest(() => {}, {
			sessionOptions: {
				customTools: [
					{
						name: "held",
						label: "held",
						description: "held writer",
						parameters: Type.Object({}),
						execute: async () => {
							started.resolve();
							await release.promise;
							settled.resolve();
							return { content: [], details: {} };
						},
					},
				],
			},
		});
		const s = runtime.session;
		const on = await s.setClmMode(true);
		faux.setResponses([fauxAssistantMessage(fauxToolCall("held", {}), { stopReason: "toolUse" })]);
		const run = s.prompt("hold");
		try {
			await started.promise;
			const open = vi.spyOn(SessionManager, "open");
			try {
				const before = readFileSync(s.sessionFile!, "utf8");
				for (const operation of [
					() => s.setClmMode(true),
					() => s.setClmMode(false),
					() => s.restoreClmRevision(on.epochId!, 0),
					() => runtime.switchSession(s.sessionFile!),
				])
					await expect(operation()).rejects.toThrow(/CLM/);
				expect(open).not.toHaveBeenCalled();
				expect(readFileSync(s.sessionFile!, "utf8")).toBe(before);
				await s.abort();
				await run;
				const aborted = readFileSync(s.sessionFile!, "utf8");
				await expect(runtime.switchSession(s.sessionFile!)).rejects.toThrow(/CLM/);
				await expect(s.setClmMode(false)).rejects.toThrow(/CLM/);
				expect(open).not.toHaveBeenCalled();
				expect(readFileSync(s.sessionFile!, "utf8")).toBe(aborted);
			} finally {
				open.mockRestore();
			}
		} finally {
			release.resolve();
			await settled.promise;
			await run;
		}
	});

	it.each(["switchSession", "importFromJsonl"] as const)(
		"reserves same-path %s against a CLM enable during shutdown",
		async (replace) => {
			const entered = createDeferred();
			const release = createDeferred();
			const { runtime, faux } = await createRuntimeForTest((pi) => {
				pi.on("session_shutdown", async (event) => {
					if (event.reason !== "resume") return;
					entered.resolve();
					await release.promise;
				});
			});
			const oldSession = runtime.session;
			await oldSession.prompt("persisted off context");
			const path = oldSession.sessionFile!;
			const replacing = runtime[replace](path);
			try {
				await entered.promise;
				const before = readFileSync(path, "utf8");
				await expect(oldSession.setClmMode(true)).rejects.toThrow(/CLM/);
				expect(SessionManager.open(path).getClmBranchState().mode).toBe("off");
				expect(readFileSync(path, "utf8")).toBe(before);
			} finally {
				release.resolve();
				await replacing;
			}
			expect((await runtime.session.getClmStatus()).mode).toBe("off");
			faux.setResponses([
				(context) => {
					expect(JSON.stringify(context.messages)).toContain("persisted off context");
					return fauxAssistantMessage("normal replacement reply");
				},
			]);
			await runtime.session.prompt("first replacement request");
			expect(getMessageText(runtime.session.messages.at(-1))).toBe("normal replacement reply");
			expect(SessionManager.open(path).getClmBranchState().mode).toBe("off");
			runtime.setBeforeSessionInvalidate(() => {
				throw new Error("held teardown failure");
			});
			try {
				await expect(runtime[replace](path)).rejects.toThrow("held teardown failure");
			} finally {
				runtime.setBeforeSessionInvalidate(undefined);
			}
			await expect(runtime.session.setClmMode(true)).resolves.toMatchObject({ mode: "on" });
		},
	);

	it("rejects enabling CLM during an action or over malformed context before mode persistence", async () => {
		const h = await createHarness({ persistSession: true });
		cleanups.push(h.cleanup);
		const started = createDeferred();
		const release = createDeferred();
		h.setResponses([
			async () => {
				started.resolve();
				await release.promise;
				return fauxAssistantMessage("done");
			},
		]);
		const run = h.session.prompt("active");
		try {
			await started.promise;
			const before = h.sessionManager.getEntries();
			await expect(h.session.setClmMode(true)).rejects.toThrow(/CLM.*idle/);
			expect(h.sessionManager.getEntries()).toEqual(before);
		} finally {
			release.resolve();
			await run;
		}
		h.sessionManager.appendMessage({
			role: "toolResult",
			toolCallId: "orphan",
			toolName: "missing",
			content: [],
			isError: false,
			timestamp: 1,
		});
		const before = h.sessionManager.getEntries();
		await expect(h.session.setClmMode(true)).rejects.toThrow();
		expect(h.sessionManager.getEntries()).toEqual(before);
	});

	it.each(["restore", "off"] as const)(
		"recovers via %s after the accepted CLM context exceeds a smaller model budget",
		async (recovery) => {
			const models = [
				{ id: "roomy", contextWindow: 1_000_000, maxTokens: 1_000 },
				{ id: "small", contextWindow: 20_000, maxTokens: 1_000 },
			];
			const settings = {
				retry: { enabled: false },
				autoRefine: { enabled: false },
				compaction: { enabled: false, keepRecentTokens: 1 },
			};
			const h = await createHarness({ persistSession: true, models, settings });
			cleanups.push(h.cleanup);
			const s = h.session;
			h.setResponses([fauxAssistantMessage("first"), fauxAssistantMessage("second")]);
			await s.prompt("obsolete audit");
			const on = await s.setClmMode(true);
			await s.prompt("accepted large history ".repeat(6_000));
			const controller = await LiveContextController.open(
				h.sessionManager,
				h.sessionManager.getSessionArtifactDir()!,
			);
			await controller.prepareToolAction("accept-large", new Map());
			const { snapshot } = h.sessionManager.captureClmContext();
			snapshot.blocks = snapshot.blocks.filter((block) => !JSON.stringify(block).includes("obsolete audit"));
			writeFileSync(on.path, renderContextDocument(snapshot, new Set()));
			await controller.settleToolAction("accept-large", new Map(), {
				model: h.getModel(),
				tools: [],
				systemPrompt: "",
			});
			await s.setModel(h.models[1]);
			const dispatched = h.faux.state.callCount;
			await s.prompt("short request");
			expect(h.faux.state.callCount).toBe(dispatched);
			const failed = h.sessionManager
				.getBranch()
				.find(
					(entry) =>
						entry.type === "message" &&
						entry.message.role === "assistant" &&
						entry.message.stopReason === "error",
				)!;
			expect(failed).toMatchObject({ message: { errorMessage: expect.stringMatching(/CLM.*budget/) } });
			expect(h.sessionManager.getBranch().at(-1)).toMatchObject({
				type: "clm_revision",
				auditWatermark: failed.id,
				observedActionId: `failed-provider:${failed.id}`,
			});
			await expect(s.getClmStatus()).resolves.toMatchObject({ mode: "on", revision: 2 });
			await s.disposeAsync();
			const resumed = await createHarness({ existingSessionFile: s.sessionFile!, models, settings });
			cleanups.push(resumed.cleanup);
			expect(resumed.sessionManager.getEntry(failed.id)).toEqual(failed);
			expect(JSON.stringify(resumed.sessionManager.buildSessionContext().messages)).toContain("short request");
			expect(resumed.sessionManager.buildSessionContext().messages).not.toContainEqual(
				expect.objectContaining({ stopReason: "error" }),
			);
			await resumed.session.setModel(resumed.models[1]);
			await expect(resumed.session.getClmStatus()).resolves.toMatchObject({ mode: "on", revision: 2 });
			if (recovery === "restore") {
				await resumed.session.restoreClmRevision(on.epochId!, 0);
			} else {
				await resumed.session.setClmMode(false);
				resumed.setResponses([
					(context) => {
						expect(JSON.stringify(context.messages)).toContain("accepted large history");
						expect(JSON.stringify(context.messages)).not.toContain("obsolete audit");
						return fauxAssistantMessage("accepted summary");
					},
				]);
				await resumed.session.compact();
			}
			resumed.setResponses([fauxAssistantMessage("recovered reply")]);
			await resumed.session.prompt("continue safely");
			expect(getMessageText(resumed.session.messages.at(-1))).toBe("recovered reply");
			expect(resumed.faux.state.callCount).toBe(recovery === "restore" ? 1 : 2);
			const reopened = SessionManager.open(resumed.session.sessionFile!);
			expect(reopened.getEntry(failed.id)).toEqual(failed);
			expect(reopened.getClmBranchState().mode).toBe(recovery === "restore" ? "on" : "off");
		},
	);

	it("guards on-mode navigation before audit mutation", async () => {
		const { runtime } = await createRuntimeForTest(() => {});
		await runtime.session.prompt("seed");
		const target = runtime.session.sessionManager.getLeafId()!;
		await runtime.session.setClmMode(true);
		const before = readFileSync(runtime.session.sessionFile!, "utf8");
		await expect(runtime.fork(target, { position: "at" })).rejects.toThrow(/CLM/);
		await expect(runtime.session.navigateTree(target)).rejects.toThrow(/CLM/);
		expect(readFileSync(runtime.session.sessionFile!, "utf8")).toBe(before);
	});

	it("passes session config to replacement runtimes", async () => {
		const calls: Array<Parameters<CreateAgentSessionRuntimeFactory>[0]> = [];
		const sessionConfig: AgentSessionRuntimeConfig = {
			cwd: "/tmp/session-config-cwd",
			model: "faux-2",
			tools: ["bash"],
		};
		const { runtime } = await createRuntimeForTest(() => {}, {
			sessionConfig,
			onCreateRuntime: (call) => calls.push(call),
		});

		expect(calls).toHaveLength(1);
		expect(calls[0]?.sessionConfig).toBe(sessionConfig);

		await runtime.newSession();

		expect(calls).toHaveLength(2);
		expect(calls[1]?.sessionConfig).toBe(sessionConfig);
	});

	it("copies depth across new-session parent reference edges", async () => {
		const { runtime } = await createRuntimeForTest(() => {});
		const parentSession = runtime.session.sessionFile;
		if (!parentSession) throw new Error("Missing parent session file");

		await runtime.newSession({ parentSession });

		expect(runtime.session.sessionManager.getHeader()).toMatchObject({ parentSession, rlmDepth: 0 });
	});

	it("uses effective runtime depth for a parented new session from a legacy header", async () => {
		const tempDir = join(tmpdir(), `pi-runtime-legacy-new-${Date.now()}-${Math.random().toString(36).slice(2)}`);
		const sessionManager = SessionManager.create(tempDir, join(tempDir, "sessions"));
		sessionManager.newSession({ rlmDepth: undefined });
		const parentSession = sessionManager.getSessionFile();
		if (!parentSession) throw new Error("Missing parent session file");
		const { runtime } = await createRuntimeForTest(() => {}, {
			cwd: tempDir,
			sessionManager,
			sessionOptions: { rlmDepth: 2 },
		});

		await runtime.newSession({ parentSession });

		expect(runtime.session.sessionManager.getHeader()).toMatchObject({ parentSession, rlmDepth: 2 });
	});

	it.each([false, true])(
		"uses the effective runtime depth when forking a legacy session before its first entry (inMemory=%s)",
		async (inMemory) => {
			const tempDir = join(tmpdir(), `pi-runtime-legacy-fork-${Date.now()}-${Math.random().toString(36).slice(2)}`);
			const sessionManager = inMemory
				? SessionManager.inMemory(tempDir)
				: SessionManager.create(tempDir, join(tempDir, "sessions"));
			sessionManager.newSession({ rlmDepth: undefined });
			const firstEntry = sessionManager.appendMessage({ role: "user", content: "fork here", timestamp: 1 });
			const { runtime } = await createRuntimeForTest(() => {}, {
				cwd: tempDir,
				sessionManager,
				sessionOptions: { rlmDepth: 2 },
			});

			await runtime.fork(firstEntry);

			expect(runtime.session.sessionManager.getHeader()?.rlmDepth).toBe(2);
			expect(runtime.session.rlmDepth).toBe(2);
		},
	);

	it("disposes a runtime only once across repeated teardown calls", async () => {
		const shutdownEvents: SessionShutdownEvent[] = [];
		const beforeInvalidate = vi.fn();
		const { runtime } = await createRuntimeForTest((pi: ExtensionAPI) => {
			pi.on("session_shutdown", (event) => {
				shutdownEvents.push(event);
			});
		});
		runtime.setBeforeSessionInvalidate(beforeInvalidate);

		await Promise.all([runtime.dispose(), runtime.dispose()]);
		await runtime.dispose();

		expect(shutdownEvents).toEqual([{ type: "session_shutdown", reason: "quit" }]);
		expect(beforeInvalidate).toHaveBeenCalledTimes(1);
	});

	it("does not replay shutdown events when runtime disposal throws", async () => {
		let shutdownCount = 0;
		const { runtime, disposeSession } = createRuntimeWithFakeSession({
			onShutdown: () => {
				shutdownCount += 1;
			},
		});
		runtime.setBeforeSessionInvalidate(() => {
			throw new Error("invalidate failed");
		});

		await expect(runtime.dispose()).rejects.toThrow("invalidate failed");
		await expect(runtime.dispose()).rejects.toThrow("invalidate failed");

		expect(shutdownCount).toBe(1);
		expect(disposeSession).toHaveBeenCalledTimes(1);
	});

	it("continues disposing tracked subagents after one child dispose fails", async () => {
		const { runtime } = createRuntimeWithFakeSession();
		const firstDispose = vi.fn(async () => {
			throw new Error("first child failed");
		});
		const secondDispose = vi.fn(async () => {});
		const firstChild = { dispose: firstDispose } as unknown as AgentSessionRuntime;
		const secondChild = { dispose: secondDispose } as unknown as AgentSessionRuntime;
		const runtimeWithSubagents = runtime as unknown as RuntimeSubagentMapAccess;
		runtimeWithSubagents.subagentRuntimes.set("first", firstChild);
		runtimeWithSubagents.subagentRuntimes.set("second", secondChild);

		await expect(runtime.dispose()).rejects.toThrow("first child failed");

		expect(firstDispose).toHaveBeenCalledTimes(1);
		expect(secondDispose).toHaveBeenCalledTimes(1);
	});

	it("deletes exact and replaced in-process RLM child runtimes and retained sessions", async () => {
		const { runtime } = await createRuntimeForTest(() => {});
		const childSession = {} as AgentSession;
		const disposeRuntime = vi.fn(async () => {});
		const childRuntime = { session: childSession, dispose: disposeRuntime } as unknown as AgentSessionRuntime;
		const runtimeWithSubagents = runtime as unknown as RuntimeSubagentMapAccess;
		runtimeWithSubagents.subagentRuntimes.set("child-1", childRuntime);

		await runtime.deleteRlmSubagentRuntime("child-1", childSession);

		expect(disposeRuntime).toHaveBeenCalledOnce();
		expect(runtimeWithSubagents.subagentRuntimes.has("child-1")).toBe(false);

		const currentSession = {} as AgentSession;
		const disposeReplacedRuntime = vi.fn(async () => {});
		const replacedRuntime = {
			session: currentSession,
			dispose: disposeReplacedRuntime,
		} as unknown as AgentSessionRuntime;
		const disposeStaleSession = vi.fn(async () => {});
		const staleSession = { disposeAsync: disposeStaleSession } as unknown as AgentSession;
		runtimeWithSubagents.subagentRuntimes.set("replaced-child", replacedRuntime);

		await runtime.deleteRlmSubagentRuntime("replaced-child", staleSession);

		expect(disposeReplacedRuntime).toHaveBeenCalledOnce();
		expect(disposeStaleSession).toHaveBeenCalledOnce();
		expect(runtimeWithSubagents.subagentRuntimes.has("replaced-child")).toBe(false);

		const disposeRetained = vi.fn(async () => {});
		const retainedSession = { disposeAsync: disposeRetained } as unknown as AgentSession;
		await runtime.deleteRlmSubagentRuntime("retained-child", retainedSession);
		expect(disposeRetained).toHaveBeenCalledOnce();
	});

	it("publishes in-process RLM sessions before create resolves and rejects cancelled startup", async () => {
		const { runtime, faux, tempDir } = await createRuntimeForTest(() => {});
		const parentSession = runtime.session;
		const getRunStatus = vi.spyOn(parentSession, "getRlmChildRunStatus").mockReturnValue("running");
		let createResolved = false;
		const onSessionPublished = vi.fn((session: AgentSession) => {
			expect(createResolved).toBe(false);
			expect(session.sessionName).toBe("in-process-worker");
		});
		const baseOptions = {
			parentSession,
			prompt: "run in process",
			model: faux.getModel(),
			thinkingLevel: "off" as const,
			serviceTier: null,
			scopedModels: [],
			activeToolNames: [],
			customTools: [],
			includeGoals: false,
			includeCompactSkill: false,
			rlmDepth: 1,
			rlmMaxDepth: 2,
		};

		const childRuntime = await runtime.createRlmSubagentRuntime({
			...baseOptions,
			id: "in-process-child",
			sessionName: "in-process-worker",
			sessionDir: join(tempDir, "in-process-child"),
			rlmParentNodeId: "in-process-child",
			onSessionPublished,
		});
		createResolved = true;
		expect(onSessionPublished).toHaveBeenCalledOnce();
		await runtime.deleteRlmSubagentRuntime("in-process-child", childRuntime.session);

		getRunStatus.mockReturnValue("cancelled");
		await expect(
			runtime.createRlmSubagentRuntime({
				...baseOptions,
				id: "cancelled-child",
				sessionName: "cancelled-worker",
				sessionDir: join(tempDir, "cancelled-child"),
				rlmParentNodeId: "cancelled-child",
			}),
		).rejects.toThrow("startup was cancelled");
		expect((runtime as unknown as RuntimeSubagentMapAccess).subagentRuntimes.has("cancelled-child")).toBe(false);
	});

	it("releases a failed child run from the inline runtime host", async () => {
		const { runtime } = await createRuntimeForTest(() => {});
		const deleteRlmSubagentRuntime = vi.spyOn(runtime, "deleteRlmSubagentRuntime");
		vi.spyOn(runtime, "createRlmSubagentRuntime").mockImplementationOnce(async (options) => {
			const childRuntime = await AgentSessionRuntime.prototype.createRlmSubagentRuntime.call(runtime, options);
			vi.spyOn(childRuntime.session, "promptAndWait").mockRejectedValue(new Error("child run failed"));
			return childRuntime;
		});

		await runtime.session.runRlmChild("fail after startup");

		await vi.waitFor(() => expect(deleteRlmSubagentRuntime).toHaveBeenCalledOnce());
		expect(runtime.listSubagentRuntimes()).toEqual([]);
	});

	it("plumbs the parent agent identity into runtime-created child prompts", async () => {
		const { runtime } = await createRuntimeForTest(() => {});
		runtime.session.setSessionName("parent-worker");
		const childRuntime = await runtime.createRlmSubagentRuntime({
			parentSession: runtime.session,
			id: "parent-agent-child",
			prompt: "inspect parent identity",
			sessionName: "child-worker",
			sessionDir: join(runtime.cwd, "parent-agent-child"),
			model: runtime.session.model!,
			thinkingLevel: "off",
			serviceTier: null,
			scopedModels: [],
			activeToolNames: [],
			customTools: [],
			includeGoals: false,
			includeCompactSkill: false,
			rlmDepth: 1,
			rlmMaxDepth: 2,
			rlmParentNodeId: "parent-agent-child",
		});

		expect(childRuntime.session.systemPrompt).toContain("spawned by parent-worker");
		await runtime.deleteRlmSubagentRuntime("parent-agent-child", childRuntime.session);
	});

	it("plumbs semantic-edge ancestry into runtime-created child ledgers", async () => {
		const { runtime, tempDir } = await createRuntimeForTest(() => {});
		const spawnedByRequestId = "a".repeat(32);
		const sessionDir = join(tempDir, "lineage-child");
		const childRuntime = await runtime.createRlmSubagentRuntime({
			parentSession: runtime.session,
			id: "lineage-child",
			prompt: "carry ancestry",
			sessionName: "lineage-worker",
			sessionDir,
			model: runtime.session.model!,
			thinkingLevel: "off",
			serviceTier: null,
			scopedModels: [],
			activeToolNames: [],
			customTools: [],
			includeGoals: false,
			includeCompactSkill: false,
			rlmDepth: 1,
			rlmMaxDepth: 2,
			rlmParentNodeId: "lineage-child",
			spawnedByRequestId,
		});

		expect(readSemanticEdgeLedger(join(sessionDir, SEMANTIC_EDGES_LEDGER_FILENAME))[0]).toMatchObject({
			type: "session_registered",
			parent_session_id: runtime.session.sessionId,
			spawned_by_request_id: spawnedByRequestId,
		});
		await runtime.deleteRlmSubagentRuntime("lineage-child", childRuntime.session);
	});

	it("keeps semantic spawn lineage through the production runtime factory", async () => {
		const tempDir = join(tmpdir(), `pi-runtime-factory-${Date.now()}-${Math.random().toString(36).slice(2)}`);
		vi.stubEnv("HOME", tempDir);
		cleanups.push(() => {
			vi.unstubAllEnvs();
		});
		mkdirSync(tempDir, { recursive: true });
		cleanups.push(() => rmSync(tempDir, { recursive: true, force: true }));
		const faux = registerFauxProvider({ models: [{ id: "faux-1", reasoning: false }] });
		cleanups.push(() => faux.unregister());

		// The PRODUCTION factory (daemon workers and runtime hosts create every
		// session through it), not a test factory that forwards all options: the
		// original defect lived in its sessionOptions whitelist and stayed
		// invisible to factory-boundary assertions.
		const factory = createDefaultRuntimeFactory(
			{
				agentDir: tempDir,
				cwd: tempDir,
				sessionDir: join(tempDir, "sessions"),
				noSkills: true,
				noPromptTemplates: true,
				noThemes: true,
				telemetryDisabled: true,
			},
			[
				(pi: ExtensionAPI) => {
					pi.registerProvider(faux.getModel().provider, {
						baseUrl: faux.getModel().baseUrl,
						apiKey: "faux-key",
						api: faux.api,
						models: faux.models.map((registeredModel) => ({
							id: registeredModel.id,
							name: registeredModel.name,
							api: registeredModel.api,
							reasoning: registeredModel.reasoning,
							input: registeredModel.input,
							cost: registeredModel.cost,
							contextWindow: registeredModel.contextWindow,
							maxTokens: registeredModel.maxTokens,
						})),
					});
				},
			],
		);

		const childSessionDir = join(tempDir, "lineage-child");
		const spawnedByRequestId = "a".repeat(32);
		const created = await factory({
			cwd: tempDir,
			agentDir: tempDir,
			sessionManager: SessionManager.create(tempDir, childSessionDir),
			sessionStartEvent: { type: "session_start", reason: "startup" },
			sessionOptions: {
				model: faux.getModel(),
				thinkingLevel: "off",
				rlmDepth: 1,
				rlmMaxDepth: 2,
				rlmSessionDir: childSessionDir,
				rlmParentNodeId: "lineage-child",
				rlmParentAgent: "parent-worker",
				semanticParentSessionId: "parent-session-id",
				semanticSpawnedByRequestId: spawnedByRequestId,
			},
		});
		cleanups.push(() => created.session.dispose());
		expect(created.services.modelRegistry.authStorage).toBe(created.services.authStorage);
		expect(created.services.authStorage.getPrimeCliConfigPath()).toBe(join(tempDir, ".prime", "config.json"));
		await created.session.bindExtensions({});

		const ledgerPath = join(childSessionDir, SEMANTIC_EDGES_LEDGER_FILENAME);
		expect(readSemanticEdgeLedger(ledgerPath)[0]).toMatchObject({
			type: "session_registered",
			parent_session_id: "parent-session-id",
			spawned_by_request_id: spawnedByRequestId,
		});

		faux.setResponses([fauxAssistantMessage("child work")]);
		await created.session.prompt("do the work");
		const edges = deriveSemanticEdges([readSemanticEdgeLedger(ledgerPath)]).edges;
		expect(edges.filter((edge) => edge.type === "subagent_call")).toMatchObject([
			{ source_request_id: spawnedByRequestId },
		]);
	});

	it("disposes hosted RLM children during session replacement", async () => {
		const disposeRlmSubagentRuntimes = vi.fn(async () => {});
		const host: SubagentRuntimeHost = {
			createRlmSubagentRuntime: async () => {
				throw new Error("unexpected child creation");
			},
			deleteRlmSubagentRuntime: async () => {},
			disposeRlmSubagentRuntimes,
		};
		const { runtime } = await createRuntimeForTest(() => {});
		runtime.setSubagentRuntimeHost(host);

		await runtime.newSession();

		expect(disposeRlmSubagentRuntimes).toHaveBeenCalledTimes(1);
	});

	it("persists message_end assistant replacements to the session manager", async () => {
		const { runtime } = await createRuntimeForTest((pi: ExtensionAPI) => {
			pi.on("message_end", (event) => {
				if (event.message.role !== "assistant") return;

				return {
					message: {
						...event.message,
						usage: {
							...event.message.usage,
							cost: {
								...event.message.usage.cost,
								total: 0.123,
							},
						},
					},
				};
			});
		});

		await runtime.session.prompt("hello");

		const sessionAssistant = runtime.session.messages.find((message) => message.role === "assistant");
		expect(sessionAssistant?.role).toBe("assistant");
		if (sessionAssistant?.role !== "assistant") {
			throw new Error("missing assistant message");
		}
		expect(sessionAssistant.usage.cost.total).toBe(0.123);

		const persistedAssistant = runtime.session.sessionManager
			.getEntries()
			.filter((entry) => entry.type === "message")
			.map((entry) => entry.message)
			.find((message) => message.role === "assistant");
		expect(persistedAssistant?.role).toBe("assistant");
		if (persistedAssistant?.role !== "assistant") {
			throw new Error("missing persisted assistant message");
		}
		expect(persistedAssistant.usage.cost.total).toBe(0.123);
	});

	it("emits session_before_switch and session_start for new and resume flows", async () => {
		const events: RecordedSessionEvent[] = [];
		const { runtime } = await createRuntimeForTest((pi: ExtensionAPI) => {
			pi.on("session_before_switch", (event) => {
				events.push(event);
			});
			pi.on("session_shutdown", (event) => {
				events.push(event);
			});
			pi.on("session_start", (event) => {
				events.push(event);
			});
		});

		expect(events).toEqual([{ type: "session_start", reason: "startup" }]);
		events.length = 0;

		await runtime.session.prompt("hello");
		const originalSessionFile = runtime.session.sessionFile;
		const originalSession = runtime.session;

		const newSessionResult = await runtime.newSession();
		expect(newSessionResult.cancelled).toBe(false);
		await runtime.session.bindExtensions({});
		expect(runtime.session).not.toBe(originalSession);
		expect(conversationMessages(runtime.session)).toEqual([]);
		const secondSessionFile = runtime.session.sessionFile;
		expect(events).toEqual([
			{ type: "session_before_switch", reason: "new", targetSessionFile: undefined },
			{ type: "session_shutdown", reason: "new", targetSessionFile: secondSessionFile },
			{ type: "session_start", reason: "new", previousSessionFile: originalSessionFile },
		]);

		events.length = 0;

		const switchResult = await runtime.switchSession(originalSessionFile!);
		expect(switchResult.cancelled).toBe(false);
		await runtime.session.bindExtensions({});
		expect(events).toEqual([
			{ type: "session_before_switch", reason: "resume", targetSessionFile: originalSessionFile },
			{ type: "session_shutdown", reason: "resume", targetSessionFile: originalSessionFile },
			{ type: "session_start", reason: "resume", previousSessionFile: secondSessionFile },
		]);
	});

	it("honors session_before_switch cancellation for new and resume", async () => {
		const events: RecordedSessionEvent[] = [];
		let cancelReason: "new" | "resume" | undefined;
		const { runtime } = await createRuntimeForTest((pi: ExtensionAPI) => {
			pi.on("session_before_switch", (event) => {
				events.push(event);
				if (event.reason === cancelReason) {
					return { cancel: true };
				}
			});
			pi.on("session_start", (event) => {
				events.push(event);
			});
		});

		await runtime.session.prompt("hello");
		const originalSessionFile = runtime.session.sessionFile;

		cancelReason = "new";
		const newResult = await runtime.newSession();
		expect(newResult.cancelled).toBe(true);
		expect(runtime.session.sessionFile).toBe(originalSessionFile);

		events.length = 0;
		const otherDir = join(tmpdir(), `pi-runtime-other-${Date.now()}-${Math.random().toString(36).slice(2)}`);
		mkdirSync(otherDir, { recursive: true });
		const otherSession = SessionManager.create(otherDir, join(otherDir, "sessions"));
		otherSession.appendMessage({ role: "user", content: [{ type: "text", text: "other" }], timestamp: Date.now() });
		const otherSessionFile = otherSession.getSessionFile();
		cancelReason = "resume";
		const resumeResult = await runtime.switchSession(otherSessionFile!);
		expect(resumeResult.cancelled).toBe(true);
		expect(runtime.session.sessionFile).toBe(originalSessionFile);
	});

	it("emits session_before_fork and session_start and honors cancellation", async () => {
		const events: RecordedSessionEvent[] = [];
		let cancelNextFork = false;
		const { runtime } = await createRuntimeForTest((pi: ExtensionAPI) => {
			pi.on("session_before_fork", (event) => {
				events.push(event);
				if (cancelNextFork) {
					cancelNextFork = false;
					return { cancel: true };
				}
			});
			pi.on("session_shutdown", (event) => {
				events.push(event);
			});
			pi.on("session_start", (event) => {
				events.push(event);
			});
		});

		events.length = 0;
		await runtime.session.prompt("hello");
		const userMessage = runtime.session.getUserMessagesForForking()[0]!;
		const previousSessionFile = runtime.session.sessionFile;

		const successResult = await runtime.fork(userMessage.entryId);
		expect(successResult.cancelled).toBe(false);
		expect(successResult.selectedText).toBe("hello");
		await runtime.session.bindExtensions({});
		expect(events).toEqual([
			{ type: "session_before_fork", entryId: userMessage.entryId, position: "before" },
			{ type: "session_shutdown", reason: "fork", targetSessionFile: runtime.session.sessionFile },
			{ type: "session_start", reason: "fork", previousSessionFile },
		]);

		events.length = 0;
		cancelNextFork = true;
		const cancelResult = await runtime.fork(userMessage.entryId);
		expect(cancelResult).toEqual({ cancelled: true });
		expect(events).toEqual([{ type: "session_before_fork", entryId: userMessage.entryId, position: "before" }]);

		events.length = 0;
		cancelNextFork = true;
		const cancelAtResult = await runtime.fork("missing-entry", { position: "at" });
		expect(cancelAtResult).toEqual({ cancelled: true });
		expect(events).toEqual([{ type: "session_before_fork", entryId: "missing-entry", position: "at" }]);
	});

	it("forks before a selected middle prompt and preserves its selection metadata", async () => {
		const { runtime } = await createRuntimeForTest(() => {});
		await runtime.session.prompt("Say one");
		await runtime.session.prompt("Say two");
		await runtime.session.prompt("Say three");
		const userMessages = runtime.session.getUserMessagesForForking();
		expect(userMessages.map((message) => message.text)).toEqual(["Say one", "Say two", "Say three"]);

		const result = await runtime.fork(userMessages[1]!.entryId);

		expect(result).toEqual({ cancelled: false, selectedText: "Say two" });
		expect(
			conversationMessages(runtime.session).map((message) =>
				message.role === "user"
					? typeof message.content === "string"
						? message.content
						: message.content
								.filter((part): part is { type: "text"; text: string } => part.type === "text")
								.map((part) => part.text)
								.join("")
					: message.role,
			),
		).toEqual(["Say one", "assistant"]);
		expect(runtime.session.sessionFile).toBeDefined();
	});

	it("forks before the first prompt in-memory and preserves its selection metadata", async () => {
		const { runtime } = await createRuntimeForTest(() => {}, { inMemory: true });
		await runtime.session.prompt("Say one");
		const userMessages = runtime.session.getUserMessagesForForking();

		const result = await runtime.fork(userMessages[0]!.entryId);

		expect(result).toEqual({ cancelled: false, selectedText: "Say one" });
		expect(conversationMessages(runtime.session)).toEqual([]);
		expect(runtime.session.sessionFile).toBeUndefined();
	});

	it("duplicates the current active branch when forking at the current position", async () => {
		const { runtime } = await createRuntimeForTest(() => {});
		await runtime.session.prompt("hello");
		await runtime.session.prompt("again");

		const beforeMessages = conversationMessages(runtime.session).map((message) => ({
			role: message.role,
			text:
				message.role === "user"
					? typeof message.content === "string"
						? message.content
						: message.content
								.filter((part): part is { type: "text"; text: string } => part.type === "text")
								.map((part) => part.text)
								.join("")
					: undefined,
		}));
		const previousSessionFile = runtime.session.sessionFile;
		const leafId = runtime.session.sessionManager.getLeafId();
		expect(leafId).toBeTruthy();

		const result = await runtime.fork(leafId!, { position: "at" });
		expect(result).toEqual({ cancelled: false, selectedText: undefined });
		expect(runtime.session.sessionFile).not.toBe(previousSessionFile);
		expect(
			conversationMessages(runtime.session).map((message) => ({
				role: message.role,
				text:
					message.role === "user"
						? typeof message.content === "string"
							? message.content
							: message.content
									.filter((part): part is { type: "text"; text: string } => part.type === "text")
									.map((part) => part.text)
									.join("")
						: undefined,
			})),
		).toEqual(beforeMessages);
	});

	it("duplicates the current active branch in-memory when forking at the current position", async () => {
		const { runtime } = await createRuntimeForTest(() => {}, { inMemory: true });

		await runtime.session.prompt("hello");
		await runtime.session.prompt("again");

		const beforeMessages = conversationMessages(runtime.session).map((message) => ({
			role: message.role,
			text:
				message.role === "user"
					? typeof message.content === "string"
						? message.content
						: message.content
								.filter((part): part is { type: "text"; text: string } => part.type === "text")
								.map((part) => part.text)
								.join("")
					: undefined,
		}));
		const leafId = runtime.session.sessionManager.getLeafId();
		expect(leafId).toBeTruthy();
		expect(runtime.session.sessionFile).toBeUndefined();

		const result = await runtime.fork(leafId!, { position: "at" });
		expect(result).toEqual({ cancelled: false, selectedText: undefined });
		expect(runtime.session.sessionFile).toBeUndefined();
		expect(
			conversationMessages(runtime.session).map((message) => ({
				role: message.role,
				text:
					message.role === "user"
						? typeof message.content === "string"
							? message.content
							: message.content
									.filter((part): part is { type: "text"; text: string } => part.type === "text")
									.map((part) => part.text)
									.join("")
						: undefined,
			})),
		).toEqual(beforeMessages);
	});

	it("throws when forking with an invalid entry id", async () => {
		const { runtime } = await createRuntimeForTest(() => {});
		await expect(runtime.fork("missing-entry")).rejects.toThrow("Invalid entry ID for forking");
	});

	it("updates the runtime session cwd on cross-cwd session replacement", async () => {
		const firstDir = join(tmpdir(), `pi-runtime-cwd-a-${Date.now()}-${Math.random().toString(36).slice(2)}`);
		const secondDir = join(tmpdir(), `pi-runtime-cwd-b-${Date.now()}-${Math.random().toString(36).slice(2)}`);
		mkdirSync(firstDir, { recursive: true });
		mkdirSync(secondDir, { recursive: true });
		const { runtime, faux, tempDir } = await createRuntimeForTest(() => {}, { cwd: firstDir });
		const otherAuthStorage = AuthStorage.inMemory();
		otherAuthStorage.setRuntimeApiKey(faux.getModel().provider, "faux-key");
		const otherRuntimeOptions = {
			agentDir: tempDir,
			authStorage: otherAuthStorage,
			resourceLoaderOptions: {
				extensionFactories: [
					(pi: ExtensionAPI) => {
						pi.registerProvider(faux.getModel().provider, {
							baseUrl: faux.getModel().baseUrl,
							apiKey: "faux-key",
							api: faux.api,
							models: faux.models.map((registeredModel) => ({
								id: registeredModel.id,
								name: registeredModel.name,
								api: registeredModel.api,
								reasoning: registeredModel.reasoning,
								input: registeredModel.input,
								cost: registeredModel.cost,
								contextWindow: registeredModel.contextWindow,
								maxTokens: registeredModel.maxTokens,
							})),
						});
					},
				],
				noSkills: true,
				noPromptTemplates: true,
				noThemes: true,
			},
		};
		const createOtherRuntime: CreateAgentSessionRuntimeFactory = async ({
			cwd,
			sessionManager,
			sessionStartEvent,
		}) => {
			const services = await createAgentSessionServices({
				...otherRuntimeOptions,
				cwd,
			});
			return {
				...(await createAgentSessionFromServices({
					services,
					sessionManager,
					sessionStartEvent,
				})),
				services,
				diagnostics: services.diagnostics,
			};
		};
		const otherRuntime = await createAgentSessionRuntime(createOtherRuntime, {
			cwd: secondDir,
			agentDir: tempDir,
			sessionManager: SessionManager.create(secondDir, join(secondDir, "sessions")),
		});
		cleanups.push(async () => {
			await otherRuntime.dispose();
		});
		await otherRuntime.session.prompt("other");
		const otherSessionFile = otherRuntime.session.sessionFile!;
		await otherRuntime.dispose();

		await runtime.switchSession(otherSessionFile);

		expect(realpathSync(runtime.session.sessionManager.getCwd())).toBe(realpathSync(secondDir));
		expect(realpathSync(runtime.cwd)).toBe(realpathSync(secondDir));
	});

	it("restores model and thinking state from the destination session", async () => {
		const { runtime, faux, tempDir } = await createRuntimeForTest(() => {}, {
			bootstrapModel: false,
			bootstrapThinkingLevel: false,
		});
		const otherDir = join(tempDir, "other");
		mkdirSync(otherDir, { recursive: true });
		const otherAuthStorage = AuthStorage.inMemory();
		otherAuthStorage.setRuntimeApiKey(faux.getModel().provider, "faux-key");
		const otherRuntimeOptions = {
			agentDir: tempDir,
			authStorage: otherAuthStorage,
			resourceLoaderOptions: {
				extensionFactories: [
					(pi: ExtensionAPI) => {
						pi.registerProvider(faux.getModel().provider, {
							baseUrl: faux.getModel().baseUrl,
							apiKey: "faux-key",
							api: faux.api,
							models: faux.models.map((registeredModel) => ({
								id: registeredModel.id,
								name: registeredModel.name,
								api: registeredModel.api,
								reasoning: registeredModel.reasoning,
								input: registeredModel.input,
								cost: registeredModel.cost,
								contextWindow: registeredModel.contextWindow,
								maxTokens: registeredModel.maxTokens,
							})),
						});
					},
				],
				noSkills: true,
				noPromptTemplates: true,
				noThemes: true,
			},
		};
		const createOtherRuntime: CreateAgentSessionRuntimeFactory = async ({
			cwd,
			sessionManager,
			sessionStartEvent,
		}) => {
			const services = await createAgentSessionServices({
				...otherRuntimeOptions,
				cwd,
			});
			return {
				...(await createAgentSessionFromServices({
					services,
					sessionManager,
					sessionStartEvent,
				})),
				services,
				diagnostics: services.diagnostics,
			};
		};
		const otherRuntime = await createAgentSessionRuntime(createOtherRuntime, {
			cwd: otherDir,
			agentDir: tempDir,
			sessionManager: SessionManager.create(otherDir, join(otherDir, "sessions")),
		});
		cleanups.push(async () => {
			await otherRuntime.dispose();
		});
		await otherRuntime.session.setModel(faux.getModel("faux-2")!);
		otherRuntime.session.setThinkingLevel("off");
		await otherRuntime.session.prompt("hello");
		const targetSessionFile = otherRuntime.session.sessionFile!;
		await otherRuntime.dispose();

		await runtime.switchSession(targetSessionFile);

		expect(runtime.session.model?.id).toBe("faux-2");
		expect(runtime.session.thinkingLevel).toBe("off");
	});

	it("runs beforeSessionInvalidate after session_shutdown and before rebindSession", async () => {
		const phases: string[] = [];
		const { runtime } = await createRuntimeForTest((pi: ExtensionAPI) => {
			pi.on("session_shutdown", () => {
				phases.push("session_shutdown");
			});
		});
		const oldSession = runtime.session;
		runtime.setBeforeSessionInvalidate(() => {
			phases.push("beforeSessionInvalidate");
			expect(oldSession.extensionRunner.createContext().cwd).toBe(oldSession.sessionManager.getCwd());
		});
		runtime.setRebindSession(async () => {
			phases.push("rebindSession");
		});

		await runtime.newSession();

		expect(phases).toEqual(["session_shutdown", "beforeSessionInvalidate", "rebindSession"]);
		expect(() => oldSession.extensionRunner.createContext().cwd).toThrow("stale after session replacement or reload");
		runtime.setBeforeSessionInvalidate(undefined);
		runtime.setRebindSession(undefined);
	});

	it("releases a replacement lease when current-session teardown fails", async () => {
		vi.stubEnv(SESSION_LEASES_ENABLED_ENV, "1");
		vi.stubEnv(SESSION_LEASE_OWNER_ID_ENV, "runtime-events");
		const { runtime } = await createRuntimeForTest(() => {});
		runtime.setBeforeSessionInvalidate(() => {
			throw new Error("teardown failed");
		});

		await expect(runtime.newSession()).rejects.toThrow("teardown failed");
		runtime.setBeforeSessionInvalidate(undefined);
		const leaseRoot = join(runtime.services.agentDir, "session-leases");
		expect(readdirSync(leaseRoot).filter((entry) => entry.endsWith(".lock"))).toHaveLength(1);
	});

	/**
	 * The runtime rebinds extensions before `withSession` runs, the callback targets the replacement
	 * session, and stale pi/ctx handles from the replaced session stop working.
	 */
	describe("regression #2860: replaced session callbacks", () => {
		/** Re-create the production rebind wiring so command contexts can replace the live session. */
		async function bindCommandContextActions(runtime: AgentSessionRuntime): Promise<void> {
			const rebindSession = async (): Promise<void> => {
				const session = runtime.session;
				await session.bindExtensions({
					commandContextActions: {
						waitForIdle: () => session.agent.waitForIdle(),
						newSession: async (options) => runtime.newSession(options),
						fork: async (entryId, options) => ({ cancelled: (await runtime.fork(entryId, options)).cancelled }),
						navigateTree: async (targetId, options) => ({
							cancelled: (await session.navigateTree(targetId, { ...options })).cancelled,
						}),
						switchSession: async (sessionPath, options) => runtime.switchSession(sessionPath, options),
						reload: async () => {
							await session.reload();
						},
					},
				});
			};
			runtime.setRebindSession(rebindSession);
			await rebindSession();
		}

		function conversation(runtime: AgentSessionRuntime): string[] {
			return conversationMessages(runtime.session).map((message) => `${message.role}:${getMessageText(message)}`);
		}

		it("rebinds before withSession, targets the replacement session, and invalidates stale pi/ctx", async () => {
			const events: string[] = [];
			let staleCtxThrows = false;
			let stalePiThrows = false;
			let oldSessionFile: string | undefined;
			let replacementSessionFile: string | undefined;
			let instanceId = 0;
			const { runtime, faux } = await createRuntimeForTest((pi: ExtensionAPI) => {
				const currentInstance = ++instanceId;
				pi.on("session_start", () => {
					events.push(`start:${currentInstance}`);
				});
				pi.on("session_shutdown", () => {
					events.push(`shutdown:${currentInstance}`);
				});
				pi.registerCommand("repro", {
					description: "repro",
					handler: async (_args, ctx) => {
						oldSessionFile = ctx.sessionManager.getSessionFile();
						await ctx.newSession({
							parentSession: oldSessionFile,
							withSession: async (replacedCtx) => {
								events.push(`with:${currentInstance}`);
								replacementSessionFile = replacedCtx.sessionManager.getSessionFile();
								try {
									ctx.sessionManager.getSessionFile();
								} catch {
									staleCtxThrows = true;
								}
								try {
									pi.sendUserMessage("stale message");
								} catch {
									stalePiThrows = true;
								}
								await replacedCtx.sendUserMessage("Hello from the new session!");
							},
						});
					},
				});
			});
			faux.setResponses([fauxAssistantMessage("hello reply")]);
			await bindCommandContextActions(runtime);
			// The shared harness binds extensions once before the rebind wiring; start from that point.
			events.length = 0;

			await runtime.session.prompt("/repro");

			expect(events).toEqual(["shutdown:1", "start:2", "with:1"]);
			expect(replacementSessionFile).toBeDefined();
			expect(replacementSessionFile).not.toBe(oldSessionFile);
			expect([staleCtxThrows, stalePiThrows]).toEqual([true, true]);
			expect(conversation(runtime)).toEqual(["user:Hello from the new session!", "assistant:hello reply"]);
		});

		it("supports withSession for fork", async () => {
			const { runtime, faux } = await createRuntimeForTest((pi: ExtensionAPI) => {
				pi.registerCommand("fork-it", {
					description: "fork-it",
					handler: async (_args, ctx) => {
						const leafId = ctx.sessionManager.getLeafId();
						if (!leafId) throw new Error("Missing leaf id");
						await ctx.fork(leafId, {
							position: "at",
							withSession: async (replacedCtx) => {
								await replacedCtx.sendUserMessage("fork callback message");
							},
						});
					},
				});
			});
			faux.setResponses([fauxAssistantMessage("seed reply"), fauxAssistantMessage("fork reply")]);
			await bindCommandContextActions(runtime);

			await runtime.session.prompt("seed");
			await runtime.session.prompt("/fork-it");

			expect(conversation(runtime)).toEqual([
				"user:seed",
				"assistant:seed reply",
				"user:fork callback message",
				"assistant:fork reply",
			]);
		});

		it("supports withSession for switchSession", async () => {
			let targetSessionPath = "";
			const { runtime, faux } = await createRuntimeForTest((pi: ExtensionAPI) => {
				pi.registerCommand("switch-it", {
					description: "switch-it",
					handler: async (_args, ctx) => {
						await ctx.switchSession(targetSessionPath, {
							withSession: async (replacedCtx) => {
								await replacedCtx.sendUserMessage("switch callback message");
							},
						});
					},
				});
			});
			faux.setResponses([
				fauxAssistantMessage("root reply"),
				fauxAssistantMessage("target reply"),
				fauxAssistantMessage("switch reply"),
			]);
			await bindCommandContextActions(runtime);

			await runtime.session.prompt("root");
			const originalSessionPath = runtime.session.sessionFile;
			expect((await runtime.newSession()).cancelled).toBe(false);
			await runtime.session.prompt("target");
			targetSessionPath = runtime.session.sessionFile!;
			await runtime.switchSession(originalSessionPath!);

			await runtime.session.prompt("/switch-it");

			expect(runtime.session.sessionFile).toBe(targetSessionPath);
			expect(conversation(runtime)).toEqual([
				"user:target",
				"assistant:target reply",
				"user:switch callback message",
				"assistant:switch reply",
			]);
		});
	});
});

describe("ENG-4620 fast mode settings", () => {
	let harness: Harness | undefined;
	const fastModeSessions: AgentSession[] = [];

	afterEach(() => {
		for (const session of fastModeSessions.splice(0)) {
			session.dispose();
		}
		harness?.cleanup();
		harness = undefined;
	});

	/** Codex models support the priority tier; gpt-5.3 and gpt-4-turbo do not. */
	async function createFastModeHarness(
		models: string[],
		options: { api?: string; provider?: string; persistSession?: boolean } = {},
	): Promise<Harness> {
		harness = await createHarness({
			api: options.api ?? "openai-codex-responses",
			provider: options.provider ?? "openai-codex",
			models: models.map((id) => ({ id })),
			persistSession: options.persistSession,
		});
		return harness;
	}

	function createFastModeSession(
		current: Harness,
		modelId?: string,
		sessionManager = SessionManager.inMemory(current.tempDir),
	) {
		return createAgentSession({
			cwd: current.tempDir,
			authStorage: current.authStorage,
			model: modelId === undefined ? current.getModel() : current.getModel(modelId),
			resourceLoader: createTestResourceLoader(),
			sessionManager,
			settingsManager: current.settingsManager,
		});
	}

	it.each([
		{
			name: "explicit model selection clamps an unsupported model and restores the preference",
			api: "openai-codex-responses",
			provider: "openai-codex",
			models: ["gpt-5.4", "gpt-5.3"],
			switchTo: async (current: Harness, modelId: string) => {
				await current.session.setModel(current.getModel(modelId)!);
				return current.session.serviceTier;
			},
		},
		{
			name: "cycling models returns the effective service tier",
			api: "openai-codex-responses",
			provider: "openai-codex",
			models: ["gpt-5.4", "gpt-5.3"],
			switchTo: async (current: Harness, modelId: string) => {
				const result = await current.session.cycleModel();
				expect(result?.model.id).toBe(modelId);
				return result?.serviceTier;
			},
		},
		{
			name: "OpenAI API-key models admit fast mode and clamp unsupported ones",
			api: "openai-responses",
			provider: "openai",
			models: ["gpt-5.5", "gpt-4-turbo"],
			switchTo: async (current: Harness, modelId: string) => {
				await current.session.setModel(current.getModel(modelId)!);
				return current.session.serviceTier;
			},
		},
	])("$name", async ({ api, provider, models, switchTo }) => {
		const [supported, unsupported] = models as [string, string];
		const current = await createFastModeHarness(models, { api, provider });

		current.session.setServiceTier("priority");
		expect(current.session.serviceTier).toBe("priority");

		expect(await switchTo(current, unsupported)).toBe("default");
		// The clamp is transient: the saved preference is untouched.
		expect(current.settingsManager.getDefaultServiceTier()).toBe("priority");

		expect(await switchTo(current, supported)).toBe("priority");
	});

	it("a clamped tier request keeps the requested preference for the parent and its children", async () => {
		const current = await createFastModeHarness(["gpt-5.5", "gpt-4-turbo"], {
			api: "openai-responses",
			provider: "openai",
		});

		current.session.setServiceTier("priority");
		expect(current.settingsManager.getDefaultServiceTier()).toBe("priority");

		await current.session.setModel(current.getModel("gpt-4-turbo")!);
		expect(current.session.serviceTier).toBe("default");

		current.session.setServiceTier("flex");
		expect(current.session.serviceTier).toBe("default");
		expect(current.settingsManager.getDefaultServiceTier()).toBe("priority");

		current.setResponses([fauxAssistantMessage("child answer")]);
		const published = new Promise<string>((resolve) =>
			current.session.subscribe((event) => {
				if (event.type === "rlm_child_update" && event.child.status === "running") resolve(event.child.id);
			}),
		);
		void current.session.runRlmChild("do the work", { model: "openai/gpt-5.5" });
		const child = current.session.getRlmChildSession(await published);
		expect(child!.sessionManager.buildSessionContext().serviceTier).toBe("flex");
		await current.session.setModel(current.getModel("gpt-5.5")!);
		expect(current.session.serviceTier).toBe("flex");
	});

	it("persists the preference across settings manager restarts", async () => {
		const current = await createFastModeHarness(["gpt-5.4"]);
		const agentDir = join(current.tempDir, "agent");
		const settingsManager = SettingsManager.create(current.tempDir, agentDir);

		settingsManager.setDefaultServiceTier("priority");
		await settingsManager.flush();

		expect(SettingsManager.create(current.tempDir, agentDir).getDefaultServiceTier()).toBe("priority");
	});

	it("uses the saved preference for new sessions and preserves it across tree navigation", async () => {
		const current = await createFastModeHarness(["gpt-5.4"]);
		const targetId = current.sessionManager.appendMessage(userMsg("first"));
		current.sessionManager.appendMessage(assistantMsg("reply"));
		current.session.setServiceTier("priority");
		current.sessionManager.appendMessage(userMsg("second"));
		expect(current.settingsManager.getDefaultServiceTier()).toBe("priority");

		await current.session.navigateTree(targetId, { summarize: false });
		expect(current.session.serviceTier).toBe("priority");

		const { session } = await createFastModeSession(current);
		fastModeSessions.push(session);
		expect(session.serviceTier).toBe("priority");

		session.setServiceTier("default");
		const { session: nextSession } = await createFastModeSession(current);
		fastModeSessions.push(nextSession);
		expect(nextSession.serviceTier).toBe("default");
	});

	it.each([
		{
			name: "a temporary clamp is not persisted into a resumed session",
			persistSession: true,
			setup: async (current: Harness) => {
				current.sessionManager.appendMessage(userMsg("hello"));
				current.session.setServiceTier("priority");
				await current.session.setModel(current.getModel("gpt-5.3")!);
				expect(current.session.serviceTier).toBe("default");
				current.session.dispose();
				return current.sessionManager;
			},
		},
		{
			name: "a new session starting on an unsupported model still stores the preference",
			persistSession: false,
			setup: async (current: Harness) => {
				const sessionManager = SessionManager.inMemory(current.tempDir);
				current.settingsManager.setDefaultServiceTier("priority");
				const { session } = await createFastModeSession(current, "gpt-5.3", sessionManager);
				fastModeSessions.push(session);
				expect(session.serviceTier).toBe("default");
				session.dispose();
				return sessionManager;
			},
		},
	])("$name", async ({ persistSession, setup }) => {
		const current = await createFastModeHarness(["gpt-5.4", "gpt-5.3"], { persistSession });

		const sessionManager = await setup(current);

		expect(sessionManager.buildSessionContext().serviceTier).toBe("priority");
		const { session: supportedSession } = await createFastModeSession(current, "gpt-5.4", sessionManager);
		fastModeSessions.push(supportedSession);
		expect(supportedSession.serviceTier).toBe("priority");
	});
});
