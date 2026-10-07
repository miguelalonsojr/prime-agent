import { createHash, randomUUID } from "node:crypto";
import { closeSync, fsyncSync, lstatSync, openSync, realpathSync, unlinkSync } from "node:fs";
import { mkdir, open, rename, rm, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { isDeepStrictEqual } from "node:util";
import type { AgentMessage } from "@earendil-works/pi-agent-core";
import type { Api, Model, Tool } from "@earendil-works/pi-ai";
import type { ClmBlock, ClmBranchState, ClmRevisionEntry, ClmSnapshot, SessionManager } from "../session-manager.js";
import { type ClmRequestBudget, estimateClmRequestTokens } from "./budget.js";
import { parseContextDocument, renderContextDocument } from "./context-document.js";

const MAX_DOCUMENT_BYTES = 32 * 1024 * 1024;
const MAX_SNAPSHOT_BYTES = 512 * 1024 * 1024;
type ProtectedActions = ReadonlyMap<string, readonly ClmBlock[]>;
export interface ClmRequestInputs {
	systemPrompt: string;
	tools: Tool[];
	model: Model<Api>;
}
export interface ClmStatus {
	mode: "on" | "off";
	epochId?: string;
	revision?: number;
	path: string;
	estimatedOccupancy?: ClmRequestBudget;
	warning?: string;
	error?: string;
	measuredUsage?: { input: number; output: number; cacheRead: number; cacheWrite: number; cost: number };
}
interface ToolAction {
	id: string;
	protectedActions: ProtectedActions;
	base: ClmSnapshot;
	pending: ClmBlock[];
	watermark: string | null;
	leafId: string | null;
}

/** Owns the disposable mirror; SessionManager remains the only accepted revision store. */
export class LiveContextController {
	private static readonly controllers = new WeakMap<SessionManager, Promise<LiveContextController>>();
	private queue: Promise<unknown> = Promise.resolve();
	private publishedHash = "";
	private action?: ToolAction;
	private conflict?: string;
	private status: ClmStatus;
	private cancelled?: { settled: boolean };
	private markerArmed = false;

	private constructor(
		private readonly sessionManager: SessionManager,
		path: string,
	) {
		const state = sessionManager.getClmBranchState();
		this.status = { mode: state.mode, epochId: state.epochId, revision: state.revision, path };
	}

	static async open(sessionManager: SessionManager, artifactDir: string): Promise<LiveContextController> {
		const path = resolve(artifactDir, "clm", "context.txt");
		let controller = LiveContextController.controllers.get(sessionManager);
		if (!controller) {
			controller = (async () => {
				LiveContextController.assertWriterSettled(sessionManager);
				const instance = new LiveContextController(sessionManager, path);
				const { snapshot } = instance.acceptedContext();
				await mkdir(dirname(path), { recursive: true });
				// Until the owner supplies action identities, expose no request bodies.
				await instance.publish(snapshot, new Set(snapshot.blocks.map((block) => block.id)));
				return instance;
			})();
			LiveContextController.controllers.set(sessionManager, controller);
		}
		const instance = await controller;
		if (instance.status.path !== path) throw new Error("CLM already owns a different working mirror");
		return instance;
	}

	/** Check before writing any off/on/restore transition, including after a restart. */
	static assertWriterSettled(sessionManager: SessionManager): void {
		const file = sessionManager.getSessionFile();
		if (!file) return;
		let canonical: string;
		try {
			canonical = realpathSync(file);
		} catch (error) {
			if (sessionManager.getClmBranchState().epochId) throw error;
			return;
		}
		try {
			lstatSync(`${canonical}.clm-writer`);
		} catch (error) {
			if (error instanceof Error && "code" in error && error.code === "ENOENT") return;
			throw error;
		}
		throw new Error(
			"CLM writer settlement is uncertain; retain the marker until known writer settlement and explicit repair",
		);
	}

	private markerPath(): string {
		const file = this.sessionManager.getSessionFile();
		if (!file) throw new Error("CLM writer safety requires a persistent session");
		return `${realpathSync(file)}.clm-writer`;
	}

	private syncMarkerDirectory(path: string): void {
		const fd = openSync(dirname(path), "r");
		try {
			fsyncSync(fd);
		} finally {
			closeSync(fd);
		}
	}

	private armMarker(): void {
		try {
			const path = this.markerPath();
			const fd = openSync(path, "wx", 0o600);
			try {
				fsyncSync(fd);
			} finally {
				closeSync(fd);
			}
			this.syncMarkerDirectory(path);
			this.markerArmed = true;
		} catch (cause) {
			this.conflict = "CLM writer marker creation or fsync is uncertain; repair the session";
			throw new Error(this.conflict, { cause });
		}
	}

	private clearMarker(): void {
		if (!this.markerArmed) return;
		const path = this.markerPath();
		try {
			unlinkSync(path);
			this.syncMarkerDirectory(path);
			this.markerArmed = false;
		} catch (cause) {
			// Re-arm if removal was visible but its directory sync failed.
			try {
				this.armMarker();
			} catch {
				/* Keep the in-process conflict even if storage is unavailable. */
			}
			this.conflict = "CLM writer marker removal is uncertain; repair the session";
			throw new Error(this.conflict, { cause });
		}
	}

	/** Cancellation invalidates an import synchronously, including one paused in readMirror. */
	abortToolAction(actionId: string, actualExecutionSettlement?: Promise<void>): void {
		if (this.action?.id !== actionId || this.cancelled) return;
		const cancelled = { settled: false };
		this.cancelled = cancelled;
		this.conflict = "CLM cancelled writer requires physical settlement and explicit mirror repair";
		this.status.error = this.conflict;
		if (actualExecutionSettlement) {
			void actualExecutionSettlement.then(
				() => {
					cancelled.settled = true;
				},
				() => undefined,
			);
		}
	}

	/** A restarted controller has no receipt and must not infer settlement from a hash or empty queue. */
	repairCancelledToolAction(): Promise<void> {
		return this.enqueue(async () => {
			if (!this.cancelled?.settled) throw new Error("CLM physical writer settlement is not known");
			const state = this.sessionManager.getClmBranchState();
			if (state.epochId !== this.status.epochId || state.revision !== this.status.revision)
				throw new Error("CLM accepted state changed before writer repair");
			const { snapshot } = this.acceptedContext();
			await this.publish(snapshot, new Set(snapshot.blocks.map((block) => block.id)));
			this.clearMarker();
			this.action = undefined;
			this.cancelled = undefined;
			this.conflict = undefined;
			this.status.error = undefined;
		});
	}

	getClmStatus(): ClmStatus {
		return structuredClone(this.status);
	}

	prepareToolAction(actionId: string, protectedActions: ProtectedActions): Promise<void> {
		const originals = structuredClone(protectedActions);
		return this.enqueue(async () => {
			this.assertIdle();
			if (!actionId) throw new Error("CLM tool action ID is required");
			await this.checkDrift();
			const leafId = this.sessionManager.getLeafId();
			const { snapshot, pending, watermark } = this.acceptedContext();
			const protectedIds = this.validateProtection(snapshot, originals);
			await this.publish(snapshot, protectedIds);
			if (this.sessionManager.getLeafId() !== leafId) {
				this.conflict = "CLM session changed while preparing tool action";
				throw new Error(this.conflict);
			}
			this.armMarker();
			this.action = {
				id: actionId,
				protectedActions: originals,
				base: snapshot,
				pending,
				watermark,
				leafId,
			};
		});
	}

	settleToolAction(
		actionId: string,
		protectedActions: ProtectedActions,
		requestBudget: ClmRequestInputs,
	): Promise<ClmRevisionEntry | null> {
		const originals = structuredClone(protectedActions);
		const request = structuredClone(requestBudget);
		return this.enqueue(async () => {
			this.assertHealthy();
			const action = this.action;
			if (!action || action.id !== actionId) throw new Error("CLM tool action does not own this mirror");
			if (!isDeepStrictEqual(originals, action.protectedActions))
				throw new Error("CLM protected action ownership changed during tool execution");
			if (this.sessionManager.getLeafId() !== action.leafId) {
				this.conflict = "CLM session changed during tool action; settle persistence before importing";
				throw new Error(this.conflict);
			}
			let committedId: string;
			let protectedIds: Set<string>;
			try {
				const text = await this.readMirror();
				this.assertHealthy();
				if (this.hash(text) === this.publishedHash) {
					await this.publish(action.base, this.validateProtection(action.base, originals));
					this.assertHealthy();
					this.clearMarker();
					this.action = undefined;
					return null;
				}
				if (action.pending.length > 0 && action.watermark === null)
					throw new Error("CLM cannot safely watermark an edit before this pending tool group");
				const candidate = parseContextDocument(text, action.base);
				protectedIds = this.validateProtection(candidate, originals);
				for (const id of protectedIds) {
					if (!text.split("\n").includes(`[[CTX_PROTECTED id=${encodeURIComponent(id)}]]`))
						throw new Error("CLM protected reference was relabelled");
				}
				this.checkBudget({ blocks: [...candidate.blocks, ...action.pending] }, request);
				this.checkSnapshotCap(candidate);
				this.checkDocumentSize(renderContextDocument(candidate, new Set()));
				// No awaited work between branch validation and the durable commit.
				if (this.sessionManager.getLeafId() !== action.leafId) throw new Error("CLM session changed before import");
				committedId = this.sessionManager.appendClmRevision(candidate, action.watermark, actionId);
			} catch (error) {
				if (!this.cancelled) await this.repairRejectedMirror(originals);
				throw error;
			}
			const entry = await this.publishAcceptedRevision(committedId, protectedIds);
			this.assertHealthy();
			this.clearMarker();
			this.action = undefined;
			return entry;
		});
	}

	discardFailedProviderEntry(
		failedEntry: ClmBlock,
		protectedActions: ProtectedActions,
		requestBudget: ClmRequestInputs,
	): Promise<ClmRevisionEntry> {
		const failed = structuredClone(failedEntry);
		const originals = structuredClone(protectedActions);
		const request = structuredClone(requestBudget);
		return this.enqueue(async () => {
			this.assertIdle();
			const leafId = this.sessionManager.getLeafId();
			await this.checkDrift();
			if (!(originals instanceof Map) || originals.size === 0)
				throw new Error("CLM failed provider cleanup requires an owned protected request");
			const { snapshot, pending, watermark } = this.acceptedContext(failed);
			if (pending.length > 0 || watermark !== failed.id)
				throw new Error("CLM failed provider cleanup cannot cover another pending tool group");
			const protectedIds = this.validateProtection(snapshot, originals);
			this.checkBudget(snapshot, request);
			this.checkSnapshotCap(snapshot);
			this.checkDocumentSize(renderContextDocument(snapshot, protectedIds));
			this.assertHealthy();
			if (this.sessionManager.getLeafId() !== leafId)
				throw new Error("CLM session changed before failed provider cleanup");
			const entryId = this.sessionManager.appendClmRevision(snapshot, failed.id, `failed-provider:${failed.id}`);
			return this.publishAcceptedRevision(entryId, protectedIds);
		});
	}

	projectRequest(
		protectedActionIds: ReadonlySet<string>,
		request: ClmRequestInputs & { protectedActions: ProtectedActions },
	): Promise<AgentMessage[]> {
		const ids = new Set(protectedActionIds);
		const inputs = structuredClone(request);
		return this.enqueue(async () => {
			this.assertIdle();
			await this.checkDrift();
			const { snapshot, pending } = this.acceptedContext();
			for (const id of ids) {
				if (!inputs?.protectedActions?.has(id)) throw new Error(`CLM protected action mapping missing: ${id}`);
			}
			this.validateProtection(snapshot, inputs?.protectedActions);
			const projected = { blocks: [...snapshot.blocks, ...pending] };
			this.checkBudget(projected, inputs);
			return structuredClone(projected.blocks.map((block) => block.message));
		});
	}

	private async repairRejectedMirror(originals: ProtectedActions): Promise<void> {
		try {
			// An uncertain append makes this read fail before any bytes are classified as rejected.
			this.sessionManager.getClmBranchState();
			await rename(this.status.path, `${this.status.path}.rejected-${randomUUID()}`);
			const { snapshot } = this.acceptedContext();
			await this.publish(snapshot, this.validateProtection(snapshot, originals));
			this.assertHealthy();
			this.clearMarker();
			this.action = undefined;
		} catch {
			this.conflict = "CLM repair failed or commit is uncertain; reopen and verify the session";
		}
	}

	private async publishAcceptedRevision(
		entryId: string,
		protectedIds: ReadonlySet<string>,
	): Promise<ClmRevisionEntry> {
		const entry = this.sessionManager.getEntry(entryId);
		if (entry?.type !== "clm_revision") {
			this.conflict = `CLM accepted entry ${entryId} cannot be verified; reopen and verify the session`;
			throw new Error(this.conflict);
		}
		this.status.revision = entry.revision;
		this.status.error = undefined;
		try {
			await this.publish(entry.snapshot, protectedIds);
		} catch {
			// Repair a disposable mirror from the confirmed commit, never from rejected bytes.
			try {
				await this.publish(entry.snapshot, protectedIds);
			} catch (error) {
				this.conflict = `CLM revision ${entry.revision} was accepted, but its mirror requires repair; reopen and verify the session`;
				throw new Error(this.conflict, { cause: error });
			}
		}
		return structuredClone(entry);
	}

	private enqueue<T>(operation: () => Promise<T>): Promise<T> {
		const result = this.queue.then(operation).catch((error: unknown) => {
			this.status.error = error instanceof Error ? error.message : String(error);
			throw error;
		});
		this.queue = result.catch(() => undefined);
		return result;
	}

	private assertHealthy(): void {
		if (this.conflict) throw new Error(this.conflict);
		const state = this.sessionManager.getClmBranchState();
		if (state.mode !== "on" || state.epochId !== this.status.epochId || state.revision !== this.status.revision)
			throw new Error("CLM accepted state changed; reopen the controller");
	}

	private assertIdle(): void {
		this.assertHealthy();
		if (this.action) throw new Error("CLM tool action writer has not settled");
		LiveContextController.assertWriterSettled(this.sessionManager);
	}

	private acceptedContext(failedEntry?: ClmBlock): {
		snapshot: ClmSnapshot;
		pending: ClmBlock[];
		watermark: string | null;
	} {
		const state: ClmBranchState = this.sessionManager.getClmBranchState();
		if (state.mode !== "on" || !state.snapshot) throw new Error("CLM requires an accepted on snapshot");
		const branch = this.sessionManager.getBranch();
		let boundary = branch.findIndex((entry) => entry.id === state.auditWatermark);
		if (state.auditWatermark === null) {
			for (const [index, entry] of branch.entries()) {
				if (entry.type === "clm_on" || entry.type === "clm_revision") boundary = index;
			}
		}
		const uncovered = branch
			.slice(boundary + 1)
			.filter(
				(entry) =>
					entry.type === "message" ||
					entry.type === "custom_message" ||
					(entry.type === "branch_summary" && entry.summary),
			);
		const messages = this.sessionManager.buildSessionContext().messages.slice(state.snapshot.blocks.length);
		if (messages.length !== uncovered.length) throw new Error("CLM audit projection cannot be mapped safely");
		const suffix = uncovered.map((entry, index) => ({ id: entry.id, message: messages[index] }));
		if (failedEntry) {
			const last = suffix.at(-1);
			if (
				!last ||
				!isDeepStrictEqual(last, failedEntry) ||
				last.message.role !== "assistant" ||
				last.message.stopReason !== "error" ||
				state.snapshot.blocks.some((block) => block.id === failedEntry.id)
			)
				throw new Error("CLM failed provider cleanup requires the exact last uncovered failed entry");
			suffix.pop();
		}
		if (suffix.some((block) => block.message.role === "assistant" && block.message.stopReason === "error"))
			throw new Error("CLM uncovered failed provider entry requires owned cleanup; repair the session");
		const snapshot = { blocks: [...state.snapshot.blocks, ...suffix] };
		const { editable, pending } = this.splitPendingToolGroup(snapshot, state.snapshot.blocks.length);
		const rendered = renderContextDocument(editable, new Set());
		this.checkDocumentSize(rendered);
		parseContextDocument(rendered, editable);
		// A failed-provider cleanup still covers its removed raw audit tail in the watermark.
		const completeCount = uncovered.length - pending.length;
		return {
			snapshot: structuredClone(editable),
			pending: structuredClone(pending),
			watermark: uncovered[completeCount - 1]?.id ?? state.auditWatermark,
		};
	}

	private splitPendingToolGroup(
		snapshot: ClmSnapshot,
		acceptedCount: number,
	): { editable: ClmSnapshot; pending: ClmBlock[] } {
		let index = snapshot.blocks.length - 1;
		while (index >= 0 && snapshot.blocks[index].message.role !== "assistant") index--;
		const message = snapshot.blocks[index]?.message;
		if (index >= acceptedCount && message?.role === "assistant") {
			const calls = message.content.filter((item) => item.type === "toolCall");
			const tail = snapshot.blocks.slice(index + 1);
			if (calls.length > tail.length && tail.every((block) => block.message.role === "toolResult")) {
				if (
					new Set(calls.map((call) => call.id)).size !== calls.length ||
					tail.some(
						(block, offset) =>
							block.message.role !== "toolResult" ||
							block.message.toolCallId !== calls[offset].id ||
							block.message.toolName !== calls[offset].name,
					)
				)
					throw new Error("CLM pending tool group is mismatched or reordered");
				return { editable: { blocks: snapshot.blocks.slice(0, index) }, pending: snapshot.blocks.slice(index) };
			}
		}
		return { editable: snapshot, pending: [] };
	}

	private validateProtection(snapshot: ClmSnapshot, actions: ProtectedActions): Set<string> {
		if (!(actions instanceof Map)) throw new Error("CLM protected action mappings are required");
		const blocks = new Map(snapshot.blocks.map((block) => [block.id, block]));
		const protectedIds = new Set<string>();
		for (const [actionId, originals] of actions) {
			if (!actionId || originals.length === 0) throw new Error("CLM protected action has no original blocks");
			for (const original of originals) {
				if (!isDeepStrictEqual(blocks.get(original.id), original))
					throw new Error("CLM protected block is missing or altered");
				protectedIds.add(original.id);
			}
		}
		return protectedIds;
	}

	private checkBudget(snapshot: ClmSnapshot, request: ClmRequestInputs): void {
		if (
			!request ||
			typeof request.systemPrompt !== "string" ||
			!Array.isArray(request.tools) ||
			!request.model ||
			(request.model.contextWindow !== undefined && !Number.isFinite(request.model.contextWindow)) ||
			!Number.isFinite(request.model.maxTokens)
		)
			throw new Error("CLM request budget inputs are required");
		const estimate = estimateClmRequestTokens({
			...request,
			messages: snapshot.blocks.map((block) => block.message),
		});
		this.status.estimatedOccupancy = estimate;
		this.status.warning =
			estimate.maxInputTokens !== null && estimate.estimatedInputTokens >= estimate.maxInputTokens * 0.9
				? "CLM context is near its request budget; shorten stale history"
				: undefined;
		if (estimate.maxInputTokens !== null && estimate.estimatedInputTokens > estimate.maxInputTokens)
			throw new Error("CLM request exceeds its input budget; shorten context or restore an accepted revision");
	}

	private checkSnapshotCap(candidate: ClmSnapshot): void {
		let bytes = Buffer.byteLength(JSON.stringify(candidate));
		for (const entry of this.sessionManager.getEntries()) {
			if (entry.type === "clm_on" || entry.type === "clm_revision" || entry.type === "clm_off_boundary")
				bytes += Buffer.byteLength(JSON.stringify(entry.snapshot));
		}
		if (bytes > MAX_SNAPSHOT_BYTES) throw new Error("CLM cumulative snapshots exceed the 512 MiB session cap");
	}

	private checkDocumentSize(text: string): void {
		if (Buffer.byteLength(text) > MAX_DOCUMENT_BYTES) throw new Error("CLM working document exceeds the 32 MiB cap");
	}

	private hash(text: string): string {
		return createHash("sha256").update(text).digest("hex");
	}

	private async readMirror(): Promise<string> {
		const file = await open(this.status.path, "r");
		try {
			const bytes = Buffer.alloc(MAX_DOCUMENT_BYTES + 1);
			let length = 0;
			while (length < bytes.length) {
				const result = await file.read(bytes, length, bytes.length - length, length);
				if (result.bytesRead === 0) break;
				length += result.bytesRead;
			}
			if (length > MAX_DOCUMENT_BYTES) throw new Error("CLM working document exceeds the 32 MiB cap");
			return new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes.subarray(0, length));
		} finally {
			await file.close();
		}
	}

	private async checkDrift(): Promise<void> {
		try {
			if (this.hash(await this.readMirror()) !== this.publishedHash) throw new Error("CLM pre-action file drift");
		} catch (error) {
			this.conflict = "CLM working file drift or unreadable mirror; restore after writer settlement";
			throw error;
		}
	}

	private async publish(snapshot: ClmSnapshot, protectedIds: ReadonlySet<string>): Promise<void> {
		const text = renderContextDocument(snapshot, protectedIds);
		this.checkDocumentSize(text);
		const temporary = `${this.status.path}.${randomUUID()}.tmp`;
		try {
			await writeFile(temporary, text, { flag: "wx" });
			await rename(temporary, this.status.path);
			this.publishedHash = this.hash(text);
		} finally {
			await rm(temporary, { force: true });
		}
	}
}
