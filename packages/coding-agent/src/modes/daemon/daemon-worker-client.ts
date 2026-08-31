import { createConnection, type Socket } from "node:net";
import { serializeJsonLine } from "../rpc/jsonl.js";
import { type PrivateFrame, PrivateFramedChannel } from "../session-worker/private-framing.js";
import {
	type DaemonClientMessageListener,
	type DaemonClientRequestOptions,
	DaemonSocketClosedError,
} from "./daemon-client.js";
import type {
	DaemonClosingReason,
	DaemonCommand,
	DaemonOutbound,
	DaemonPeerTransportTicket,
	DaemonResponse,
	DaemonServerCapability,
} from "./daemon-protocol.js";
import {
	type DaemonPeerCommand,
	type DaemonPeerCommandBody,
	type DaemonWorkerCommand,
	type DaemonWorkerCommandBody,
	type DaemonWorkerFrameHeader,
	isDaemonWorkerFrameHeader,
} from "./daemon-worker-protocol.js";

type DistributiveOmit<T, K extends keyof T> = T extends unknown ? Omit<T, K> : never;
type DaemonCommandBody = DistributiveOmit<DaemonCommand, "id">;
type DaemonWorkerWireCommandBody = DaemonCommandBody | DaemonWorkerCommandBody | DaemonPeerCommandBody;
type DaemonWorkerWireCommand = DaemonCommand | DaemonWorkerCommand | DaemonPeerCommand;
type DaemonWorkerAuthentication = Omit<Extract<DaemonWorkerCommand, { type: "worker_auth" }>, "id" | "type" | "token">;

export type DaemonWorkerFrameListener = (frame: PrivateFrame<DaemonWorkerFrameHeader>) => void;
export type DaemonWorkerCloseListener = (error: Error) => void;
type DaemonHello = Extract<DaemonOutbound, { type: "daemon_hello" }>;

class DaemonWorkerRequestUncertainError extends Error {
	constructor(commandType: string, cause: Error) {
		super(`Daemon worker request outcome is uncertain after write began: ${commandType}`, { cause });
		this.name = "DaemonWorkerRequestUncertainError";
	}
}

export class DaemonWorkerClient {
	private socket?: Socket;
	private channel?: PrivateFramedChannel<DaemonWorkerFrameHeader>;
	private readonly frameListeners = new Set<DaemonWorkerFrameListener>();
	private readonly messageListeners = new Set<DaemonClientMessageListener>();
	private readonly closeListeners = new Set<DaemonWorkerCloseListener>();
	private readonly pending = new Map<
		string,
		{
			resolve: (response: DaemonResponse) => void;
			reject: (error: Error) => void;
			timeout: ReturnType<typeof setTimeout>;
			commandType: string;
			writeStarted: boolean;
		}
	>();
	private requestId = 0;
	private helloMessage?: DaemonHello;
	private directPeer = false;
	private directClosingReason?: DaemonClosingReason;
	private readonly directSnapshotTransfers = new Map<string, { snapshotId: string; nextIndex: number }>();
	private readonly helloWaiters = new Set<{
		resolve: (hello: DaemonHello) => void;
		reject: (error: Error) => void;
		timeout: ReturnType<typeof setTimeout>;
	}>();

	constructor(private readonly socketPath: string) {}

	get hello(): DaemonHello | undefined {
		return this.helloMessage;
	}

	get isConnected(): boolean {
		return this.socket !== undefined && !this.socket.destroyed;
	}

	supportsServerCapability(capability: DaemonServerCapability): boolean {
		return this.helloMessage?.serverCapabilities?.includes(capability) === true;
	}

	async connect(timeoutMs = 3000): Promise<void> {
		if (this.socket) {
			throw new Error("Daemon worker client is already connected");
		}
		const socket = createConnection(this.socketPath);
		this.socket = socket;
		this.channel = new PrivateFramedChannel(socket, isDaemonWorkerFrameHeader);
		this.channel.onFrame((frame) => this.handleFrame(frame));

		await new Promise<void>((resolve, reject) => {
			const timeout = setTimeout(() => {
				cleanup();
				socket.destroy();
				reject(new Error(`Timed out connecting to daemon worker socket: ${this.socketPath}`));
			}, timeoutMs);
			const cleanup = () => {
				clearTimeout(timeout);
				socket.off("connect", onConnect);
				socket.off("error", onError);
			};
			const onConnect = () => {
				cleanup();
				resolve();
			};
			const onError = (error: Error) => {
				cleanup();
				reject(error);
			};
			socket.once("connect", onConnect);
			socket.once("error", onError);
		});

		socket.on("error", (error) => this.notifyClosed(socket, this.directCloseError(error)));
		socket.on("close", () =>
			this.notifyClosed(socket, this.directCloseError(new Error("Daemon worker socket closed"))),
		);
	}

	waitForHello(timeoutMs = 3000): Promise<DaemonHello> {
		if (this.helloMessage) {
			return Promise.resolve(this.helloMessage);
		}
		if (!this.socket || this.socket.destroyed) {
			return Promise.reject(new Error("Daemon worker client is not connected"));
		}
		return new Promise((resolve, reject) => {
			const waiter = {
				resolve,
				reject,
				timeout: setTimeout(() => {
					this.helloWaiters.delete(waiter);
					reject(new Error("Timed out waiting for daemon worker hello"));
				}, timeoutMs),
			};
			this.helloWaiters.add(waiter);
		});
	}

	onFrame(listener: DaemonWorkerFrameListener): () => void {
		this.frameListeners.add(listener);
		return () => this.frameListeners.delete(listener);
	}

	onMessage(listener: DaemonClientMessageListener): () => void {
		this.messageListeners.add(listener);
		return () => this.messageListeners.delete(listener);
	}

	onClose(listener: DaemonWorkerCloseListener): () => void {
		this.closeListeners.add(listener);
		return () => this.closeListeners.delete(listener);
	}

	request(
		command: DaemonCommandBody,
		timeoutMs = 30_000,
		_options: DaemonClientRequestOptions = {},
	): Promise<DaemonResponse> {
		return this.requestWire(command, timeoutMs);
	}

	requestWorker(command: DaemonWorkerCommandBody, timeoutMs = 30_000): Promise<DaemonResponse> {
		return this.requestWire(command, timeoutMs);
	}

	requestPeer(command: DaemonPeerCommandBody, timeoutMs = 30_000, onAdmitted?: () => void): Promise<DaemonResponse> {
		return this.requestWire(command, timeoutMs, onAdmitted);
	}

	async authenticateWorker(token: string, owner: DaemonWorkerAuthentication, timeoutMs = 3000): Promise<void> {
		const response = await this.requestWorker({ type: "worker_auth", token, ...owner }, timeoutMs);
		if (!response.success) {
			throw new Error(response.error);
		}
	}

	async authenticatePeer(ticket: DaemonPeerTransportTicket, timeoutMs = 3000): Promise<void> {
		const response = await this.requestPeer(
			{
				type: "peer_auth",
				grantId: ticket.grantId,
				token: ticket.token,
				workerInstanceId: ticket.workerInstanceId,
				purpose: ticket.purpose,
			},
			timeoutMs,
		);
		if (!response.success) throw new Error(response.error);
		const authenticated = response.data as
			| { workerId?: unknown; workerInstanceId?: unknown; purpose?: unknown }
			| undefined;
		if (
			authenticated?.workerId !== ticket.workerId ||
			authenticated.workerInstanceId !== ticket.workerInstanceId ||
			authenticated.purpose !== ticket.purpose
		) {
			throw new Error("Daemon worker returned an invalid peer authentication identity");
		}
		this.directPeer = true;
	}

	close(): void {
		this.rejectAll(new Error("Daemon worker client closed"));
		this.channel?.close();
		this.channel = undefined;
		this.socket?.destroy();
		this.socket = undefined;
		this.directPeer = false;
		this.directClosingReason = undefined;
		this.directSnapshotTransfers.clear();
	}

	private async requestWire(
		command: DaemonWorkerWireCommandBody,
		timeoutMs: number,
		onAdmitted?: () => void,
	): Promise<DaemonResponse> {
		if (!this.channel || !this.socket || this.socket.destroyed) {
			throw new Error("Daemon worker client is not connected");
		}
		const id = `worker_${++this.requestId}`;
		const fullCommand = { ...command, id } as DaemonWorkerWireCommand;
		const response = new Promise<DaemonResponse>((resolve, reject) => {
			const timeout = setTimeout(() => {
				const pending = this.pending.get(id);
				this.pending.delete(id);
				const error = new Error(`Timed out waiting for daemon worker response to ${command.type}`);
				reject(pending?.writeStarted ? new DaemonWorkerRequestUncertainError(command.type, error) : error);
			}, timeoutMs);
			this.pending.set(id, { resolve, reject, timeout, commandType: command.type, writeStarted: false });
		});
		try {
			await this.channel.send(
				{ kind: "command", requestId: id, commandType: command.type },
				Buffer.from(serializeJsonLine(fullCommand)),
				onAdmitted
					? () => {
							const pending = this.pending.get(id);
							if (pending) pending.writeStarted = true;
							onAdmitted();
						}
					: undefined,
			);
		} catch (error) {
			const pending = this.pending.get(id);
			if (pending) {
				clearTimeout(pending.timeout);
				this.pending.delete(id);
				const cause = error instanceof Error ? error : new Error(String(error));
				pending.reject(pending.writeStarted ? new DaemonWorkerRequestUncertainError(command.type, cause) : cause);
			}
		}
		return response;
	}

	private handleFrame(frame: PrivateFrame<DaemonWorkerFrameHeader>): void {
		if (frame.header.kind !== "outbound") {
			return;
		}
		if (frame.header.outboundType === "response" && frame.header.requestId) {
			const pending = this.pending.get(frame.header.requestId);
			if (pending) {
				let response: unknown;
				try {
					response = JSON.parse(frame.payload.toString("utf8"));
				} catch (error) {
					this.closeMalformedTransport(new Error(`Invalid daemon worker response: ${String(error)}`));
					return;
				}
				if (
					!isDaemonResponse(response) ||
					response.id !== frame.header.requestId ||
					((pending.commandType === "peer_auth" ||
						pending.commandType === "worker_auth" ||
						!pending.commandType.startsWith("worker_")) &&
						response.command !== pending.commandType) ||
					frame.header.payloadEncoding !== "jsonl"
				) {
					this.closeMalformedTransport(
						new Error(
							`Daemon worker response did not match its private frame (id=${String(response && typeof response === "object" && "id" in response ? response.id : undefined)}, frame=${frame.header.requestId}, command=${String(response && typeof response === "object" && "command" in response ? response.command : undefined)}, expected=${pending.commandType}, encoding=${String(frame.header.payloadEncoding)})`,
						),
					);
					return;
				}
				clearTimeout(pending.timeout);
				this.pending.delete(frame.header.requestId);
				pending.resolve(response);
				return;
			}
		}
		if (frame.header.outboundType === "daemon_hello") {
			try {
				const parsed = JSON.parse(frame.payload.toString("utf8")) as DaemonOutbound;
				if (parsed.type === "daemon_hello") {
					this.helloMessage = parsed;
					for (const waiter of [...this.helloWaiters]) {
						clearTimeout(waiter.timeout);
						this.helloWaiters.delete(waiter);
						waiter.resolve(parsed);
					}
				}
			} catch {
				// Invalid hello payloads are rejected by the timeout.
			}
		}
		for (const listener of this.frameListeners) {
			listener(frame);
		}
		if (this.directPeer && frame.header.outboundType !== "daemon_hello" && frame.header.outboundType !== "response") {
			try {
				const message = this.decodeDirectOutbound(frame);
				for (const listener of [...this.messageListeners]) listener(message);
			} catch (error) {
				const directError = error instanceof Error ? error : new Error(String(error));
				const socket = this.socket;
				if (socket) {
					this.notifyClosed(socket, directError);
					socket.destroy(directError);
				}
			}
		}
	}

	private decodeDirectOutbound(frame: PrivateFrame<DaemonWorkerFrameHeader>): DaemonOutbound {
		if (frame.header.kind !== "outbound" || frame.header.payloadEncoding !== "jsonl") {
			throw new Error("Direct worker returned an unsupported compact event frame");
		}
		let parsed: unknown;
		try {
			parsed = JSON.parse(frame.payload.toString("utf8"));
		} catch (error) {
			throw new Error(`Direct worker returned invalid JSON: ${String(error)}`);
		}
		if (!parsed || typeof parsed !== "object" || !("type" in parsed)) {
			throw new Error("Direct worker returned an invalid outbound payload");
		}
		const message = parsed as DaemonOutbound;
		if (message.type !== frame.header.outboundType) {
			throw new Error("Direct worker frame type did not match its payload");
		}
		if (
			frame.header.activeSessionId !== undefined &&
			(!("activeSessionId" in message) || message.activeSessionId !== frame.header.activeSessionId)
		) {
			throw new Error("Direct worker frame session did not match its payload");
		}
		if (
			frame.header.snapshotId !== undefined &&
			(!("snapshotId" in message) || message.snapshotId !== frame.header.snapshotId)
		) {
			throw new Error("Direct worker frame snapshot did not match its payload");
		}
		if (
			frame.header.sessionEventType !== undefined &&
			(message.type !== "session_event" || message.event.type !== frame.header.sessionEventType)
		) {
			throw new Error("Direct worker frame event type did not match its payload");
		}
		if (message.type === "daemon_closing") this.directClosingReason = message.reason;
		this.validateDirectSnapshotTransfer(message);
		return message;
	}

	private validateDirectSnapshotTransfer(message: DaemonOutbound): void {
		if (message.type === "session_snapshot_begin") {
			if (this.directSnapshotTransfers.has(message.activeSessionId)) {
				throw new Error(`Direct snapshot restarted before completion: ${message.snapshotId}`);
			}
			this.directSnapshotTransfers.set(message.activeSessionId, { snapshotId: message.snapshotId, nextIndex: 0 });
			return;
		}
		if (message.type === "session_snapshot_chunk") {
			const transfer = this.directSnapshotTransfers.get(message.activeSessionId);
			if (!transfer || transfer.snapshotId !== message.snapshotId || transfer.nextIndex !== message.index) {
				throw new Error(`Direct snapshot chunk was out of order: ${message.snapshotId}/${message.index}`);
			}
			transfer.nextIndex++;
			return;
		}
		if (message.type === "session_snapshot_end") {
			const transfer = this.directSnapshotTransfers.get(message.activeSessionId);
			if (!transfer || transfer.snapshotId !== message.snapshotId || transfer.nextIndex !== message.chunkCount) {
				throw new Error(`Direct snapshot ended with invalid metadata: ${message.snapshotId}`);
			}
			this.directSnapshotTransfers.delete(message.activeSessionId);
			return;
		}
		if (message.type === "session_snapshot_failed") {
			const transfer = this.directSnapshotTransfers.get(message.activeSessionId);
			if (!transfer || transfer.snapshotId !== message.snapshotId) {
				throw new Error(`Direct snapshot failure did not match an active transfer: ${message.snapshotId}`);
			}
			this.directSnapshotTransfers.delete(message.activeSessionId);
		}
	}

	private directCloseError(cause: Error): Error {
		return this.directPeer
			? new DaemonSocketClosedError(this.socketPath, this.directClosingReason, cause.message)
			: cause;
	}

	private closeMalformedTransport(error: Error): void {
		const socket = this.socket;
		if (!socket) return;
		this.notifyClosed(socket, error);
		socket.destroy(error);
	}

	private rejectAll(error: Error): void {
		for (const [id, pending] of this.pending) {
			clearTimeout(pending.timeout);
			pending.reject(
				pending.writeStarted ? new DaemonWorkerRequestUncertainError(pending.commandType, error) : error,
			);
			this.pending.delete(id);
		}
		for (const waiter of [...this.helloWaiters]) {
			clearTimeout(waiter.timeout);
			this.helloWaiters.delete(waiter);
			waiter.reject(error);
		}
	}

	private notifyClosed(socket: Socket, error: Error): void {
		if (this.socket !== socket) {
			return;
		}
		this.socket = undefined;
		this.channel = undefined;
		this.directPeer = false;
		this.directClosingReason = undefined;
		this.directSnapshotTransfers.clear();
		this.rejectAll(error);
		for (const listener of [...this.closeListeners]) {
			listener(error);
		}
	}
}

function isDaemonResponse(value: unknown): value is DaemonResponse {
	if (!value || typeof value !== "object") {
		return false;
	}
	const candidate = value as { type?: unknown; command?: unknown; success?: unknown };
	return (
		candidate.type === "response" && typeof candidate.command === "string" && typeof candidate.success === "boolean"
	);
}
