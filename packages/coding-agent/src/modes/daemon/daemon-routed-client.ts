import {
	DaemonCapabilityUnavailableError,
	type DaemonClientCloseListener,
	type DaemonClientMessageListener,
	type DaemonClientRequestOptions,
	type DaemonCommandBody,
	type DaemonHello,
	DaemonSocketClosedError,
	type DaemonTransportClient,
	getDaemonSocketCloseReason,
} from "./daemon-client.js";
import { parseDaemonPeerTransportTicket } from "./daemon-peer-transport-ticket.js";
import type { DaemonClosingReason, DaemonResponse, DaemonServerCapability } from "./daemon-protocol.js";
import { getDaemonSocketIdentity } from "./daemon-socket.js";
import { DaemonWorkerClient } from "./daemon-worker-client.js";
import { isDirectSessionCommand } from "./daemon-worker-protocol.js";

export class DaemonDirectTransportClosedError extends DaemonSocketClosedError {
	constructor(cause: Error) {
		super("direct-worker", getDaemonSocketCloseReason(cause), cause.message);
		this.name = "DaemonDirectTransportClosedError";
	}
}

export class DaemonControlPlaneTransportError extends Error {
	constructor(cause: Error) {
		super(`Daemon control-plane transport failed: ${cause.message}`, { cause });
		this.name = "DaemonControlPlaneTransportError";
	}
}

export class DaemonRoutedClient implements DaemonTransportClient {
	private direct?: DaemonWorkerClient;
	private readonly messageListeners = new Set<DaemonClientMessageListener>();
	private readonly closeListeners = new Set<DaemonClientCloseListener>();
	private readonly unsubscribeSupervisorMessage: () => void;
	private readonly unsubscribeSupervisorClose: () => void;
	private unsubscribeDirectMessage?: () => void;
	private unsubscribeDirectClose?: () => void;
	private closed = false;

	constructor(
		private readonly supervisor: DaemonTransportClient,
		direct: DaemonWorkerClient,
	) {
		this.direct = direct;
		this.unsubscribeSupervisorMessage = supervisor.onMessage((message) => this.emitMessage(message));
		this.unsubscribeSupervisorClose = supervisor.onClose((error) => {
			if (!this.closed) this.emitClose(error);
		});
		this.bindDirect(direct);
	}

	get hello(): DaemonHello | undefined {
		return this.supervisor.hello ?? this.direct?.hello;
	}

	get isConnected(): boolean {
		return this.supervisor.isConnected || this.direct?.isConnected === true;
	}

	get hasDirectTransport(): boolean {
		return this.direct?.isConnected === true;
	}

	get controlPlaneTransport(): DaemonTransportClient {
		return this.supervisor;
	}

	get isControlPlaneReady(): boolean {
		return this.supervisor.isConnected && this.supervisor.hello !== undefined;
	}

	supportsServerCapability(capability: DaemonServerCapability): boolean {
		return (
			this.direct?.supportsServerCapability(capability) === true ||
			this.supervisor.supportsServerCapability(capability)
		);
	}

	waitForHello(timeoutMs = 3000): Promise<DaemonHello> {
		return this.supervisor.waitForHello(timeoutMs);
	}

	async connect(timeoutMs = 3000): Promise<void> {
		if (!this.supervisor.isConnected) await this.supervisor.connect(timeoutMs);
	}

	async reconnect(timeoutMs = 3000): Promise<void> {
		if (!this.supervisor.isConnected) await this.supervisor.reconnect(timeoutMs);
	}

	disconnectForReconnect(reason: DaemonClosingReason): void {
		this.fallbackToSupervisor(false);
		this.supervisor.disconnectForReconnect(reason);
	}

	resetTransportForReconnect(): void {
		this.supervisor.resetTransportForReconnect();
	}

	onMessage(listener: DaemonClientMessageListener): () => void {
		this.messageListeners.add(listener);
		return () => this.messageListeners.delete(listener);
	}

	onClose(listener: DaemonClientCloseListener): () => void {
		this.closeListeners.add(listener);
		return () => this.closeListeners.delete(listener);
	}

	enableRequestRecovery(): void {
		this.supervisor.enableRequestRecovery();
	}

	request(
		command: DaemonCommandBody,
		timeoutMs = 30_000,
		options: DaemonClientRequestOptions = {},
	): Promise<DaemonResponse> {
		if (command.type === "reattach") {
			return this.requestControlPlane(command, timeoutMs, options).then((response) => {
				if (response.success) this.fallbackToSupervisor(false);
				return response;
			});
		}
		const direct = this.direct;
		if (direct?.isConnected && isDirectSessionCommand(command)) {
			return direct.request(command, timeoutMs, options).catch((error: unknown) => {
				if (this.direct !== direct) {
					throw new DaemonDirectTransportClosedError(error instanceof Error ? error : new Error(String(error)));
				}
				throw error;
			});
		}
		return this.requestControlPlane(command, timeoutMs, options);
	}

	private requestControlPlane(
		command: DaemonCommandBody,
		timeoutMs: number,
		options: DaemonClientRequestOptions,
	): Promise<DaemonResponse> {
		return this.supervisor.request(command, timeoutMs, options).catch((error: unknown) => {
			if (error instanceof DaemonCapabilityUnavailableError) throw error;
			throw new DaemonControlPlaneTransportError(error instanceof Error ? error : new Error(String(error)));
		});
	}

	fallbackToSupervisor(notify = false): void {
		const direct = this.direct;
		if (!direct) return;
		this.direct = undefined;
		this.unsubscribeDirectMessage?.();
		this.unsubscribeDirectMessage = undefined;
		this.unsubscribeDirectClose?.();
		this.unsubscribeDirectClose = undefined;
		direct.close();
		if (notify) this.emitClose(new DaemonDirectTransportClosedError(new Error("direct fallback requested")));
	}

	close(): void {
		if (this.closed) return;
		this.closed = true;
		this.fallbackToSupervisor(false);
		this.unsubscribeSupervisorMessage();
		this.unsubscribeSupervisorClose();
		this.supervisor.close();
		this.messageListeners.clear();
		this.closeListeners.clear();
	}

	private bindDirect(direct: DaemonWorkerClient): void {
		this.unsubscribeDirectMessage = direct.onMessage((message) => this.emitMessage(message));
		this.unsubscribeDirectClose = direct.onClose((error) => {
			if (this.closed || this.direct !== direct) return;
			this.direct = undefined;
			this.unsubscribeDirectMessage?.();
			this.unsubscribeDirectMessage = undefined;
			this.unsubscribeDirectClose = undefined;
			this.emitClose(new DaemonDirectTransportClosedError(error));
		});
	}

	private emitMessage(message: Parameters<DaemonClientMessageListener>[0]): void {
		for (const listener of [...this.messageListeners]) listener(message);
	}

	private emitClose(error: Error): void {
		for (const listener of [...this.closeListeners]) listener(error);
	}
}

export async function createDaemonSessionTransport(
	supervisor: DaemonTransportClient,
	activeSessionId: string,
	directDisabled: boolean,
): Promise<DaemonTransportClient> {
	if (
		directDisabled ||
		supervisor instanceof DaemonRoutedClient ||
		!supervisor.supportsServerCapability("direct_peer_transport")
	) {
		return supervisor;
	}
	let direct: DaemonWorkerClient | undefined;
	try {
		const response = await supervisor.request({ type: "get_direct_worker_transport", activeSessionId }, 5000);
		if (!response.success) return supervisor;
		const ticket = parseDaemonPeerTransportTicket(response.data, "session_client");
		if (!ticket || ticket.activeSessionId !== activeSessionId || Date.parse(ticket.expiresAt) <= Date.now()) {
			return supervisor;
		}
		const currentIdentity = getDaemonSocketIdentity(ticket.socketPath);
		if (
			!currentIdentity ||
			currentIdentity.dev !== ticket.socketIdentity.dev ||
			currentIdentity.ino !== ticket.socketIdentity.ino
		) {
			return supervisor;
		}
		direct = new DaemonWorkerClient(ticket.socketPath);
		await direct.connect(1000);
		await direct.waitForHello(1000);
		await direct.authenticatePeer(ticket, 1000);
		return new DaemonRoutedClient(supervisor, direct);
	} catch {
		direct?.close();
		return supervisor;
	}
}
