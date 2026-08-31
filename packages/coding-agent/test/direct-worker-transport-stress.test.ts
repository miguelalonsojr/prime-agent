import { describe, expect, it, vi } from "vitest";
import type { DaemonSocketClient } from "../src/modes/daemon/active-session-state.js";
import type {
	DaemonClientCloseListener,
	DaemonClientMessageListener,
	DaemonClientRequestOptions,
	DaemonHello,
	DaemonTransportClient,
} from "../src/modes/daemon/daemon-client.js";
import { AgentDaemon } from "../src/modes/daemon/daemon-mode.js";
import type { DaemonCommand, DaemonResponse } from "../src/modes/daemon/daemon-protocol.js";
import { DaemonRoutedClient } from "../src/modes/daemon/daemon-routed-client.js";
import type { SessionSummary } from "../src/modes/daemon/daemon-session-list.js";
import { DaemonSupervisor } from "../src/modes/daemon/daemon-supervisor.js";
import type { DaemonWorkerClient } from "../src/modes/daemon/daemon-worker-client.js";
import type { DaemonWorkerPeerGrant } from "../src/modes/daemon/daemon-worker-protocol.js";

class Deferred<T> {
	readonly promise: Promise<T>;
	private resolvePromise: (value: T) => void = () => {};

	constructor() {
		this.promise = new Promise<T>((resolve) => {
			this.resolvePromise = resolve;
		});
	}

	resolve(value: T): void {
		this.resolvePromise(value);
	}
}

class CachedListControlPlane {
	readonly hello: DaemonHello | undefined;
	readonly isConnected = true;
	private nextRequestId = 0;

	constructor(
		private readonly supervisor: {
			handleList(client: object, command: { id: string; type: "list"; refresh?: boolean }): Promise<DaemonResponse>;
		},
	) {}

	async request(command: DaemonCommand): Promise<DaemonResponse> {
		if (command.type !== "list") throw new Error(`Unexpected control-plane command: ${command.type}`);
		return this.supervisor.handleList({}, { id: `cached-list-${this.nextRequestId++}`, ...command });
	}

	supportsServerCapability(): boolean {
		return true;
	}

	onMessage(_listener: DaemonClientMessageListener): () => void {
		return () => {};
	}

	onClose(_listener: DaemonClientCloseListener): () => void {
		return () => {};
	}

	close(): void {}
}

class FakeDirectWorker {
	readonly isConnected = true;
	readonly state = new Deferred<DaemonResponse>();

	async request(
		command: DaemonCommand,
		_timeoutMs?: number,
		_options?: DaemonClientRequestOptions,
	): Promise<DaemonResponse> {
		if (command.type !== "get_state") throw new Error(`Unexpected direct command: ${command.type}`);
		return this.state.promise;
	}

	onMessage(_listener: DaemonClientMessageListener): () => void {
		return () => {};
	}

	onClose(_listener: DaemonClientCloseListener): () => void {
		return () => {};
	}

	close(): void {}
}

interface AgentMessageDelivery {
	id: string;
	message: string;
	rootActiveSessionId: string;
	targetActiveSessionId: string;
	targetSessionId: string;
	senderActiveSessionId: string;
	senderSessionId: string;
}

interface AgentMessageRequest {
	targetSelector: string;
	message: string;
	sender: { activeSessionId?: string; sessionId?: string; clientId: string };
	senderKey: string;
	origin: "agent";
}

interface AgentDaemonFixture {
	daemon: { handleLine(client: DaemonSocketClient, line: string): Promise<void> };
	clients: DaemonSocketClient[];
	deliveries: AgentMessageDelivery[];
	stalledDelivery: Deferred<{ deliveryStatus: "queued" }>;
}

function success(command: DaemonCommand["type"]): DaemonResponse {
	return { type: "response", command, success: true } as DaemonResponse;
}

function createCachedListControlPlane(): {
	controlPlane: CachedListControlPlane;
	releaseRefresh(): void;
} {
	const activeRefresh = new Deferred<void>();
	const workerClient = {};
	const summary = {
		id: "cached-active",
		activeSessionId: "cached-active",
		sessionId: "cached-session",
		cwd: "/tmp",
	} as unknown as SessionSummary;
	const worker = {
		descriptor: {
			workerId: "cached-worker",
			pid: process.pid,
			rootActiveSessionId: "cached-active",
			lifecycle: "ready" as const,
		},
		client: workerClient,
		intentionalStop: false,
		summaries: new Map([["cached-active", summary]]),
		summaryRefresh: { promise: activeRefresh.promise, client: workerClient, recovery: false, generation: 1 },
	};
	const supervisor = Object.assign(Object.create(DaemonSupervisor.prototype), {
		workers: new Map([[worker.descriptor.workerId, worker]]),
		clients: new Set(),
		log: vi.fn(),
	}) as {
		handleList(client: object, command: { id: string; type: "list"; refresh?: boolean }): Promise<DaemonResponse>;
	};

	return {
		controlPlane: new CachedListControlPlane(supervisor),
		releaseRefresh: () => activeRefresh.resolve(),
	};
}

function routedClient(controlPlane: CachedListControlPlane, worker: FakeDirectWorker): DaemonRoutedClient {
	return new DaemonRoutedClient(
		controlPlane as unknown as DaemonTransportClient,
		worker as unknown as DaemonWorkerClient,
	);
}

function makePeerClient(id: string): DaemonSocketClient {
	return {
		id,
		authenticated: true,
		authenticationRole: "worker_peer",
		transport: "private-framed",
		socket: { destroyed: false, write: vi.fn(() => true), end: vi.fn() },
		attachedActiveSessionIds: new Set(),
		detachInput: vi.fn(),
		supportsExtensionUi: false,
		capabilities: new Set(),
	} as unknown as DaemonSocketClient;
}

function createAgentDaemonFixture(rootIndex: number, stalled: boolean): AgentDaemonFixture {
	const rootActiveSessionId = `root-${rootIndex}`;
	const stalledDelivery = new Deferred<{ deliveryStatus: "queued" }>();
	const deliveries: AgentMessageDelivery[] = [];
	const clients = Array.from({ length: 4 }, (_, deliveryIndex) =>
		makePeerClient(`peer-${rootIndex}-${deliveryIndex}`),
	);
	const grants = clients.map((client, deliveryIndex) => {
		const targetActiveSessionId = `${rootActiveSessionId}-target-${deliveryIndex}`;
		const targetSessionId = `${rootActiveSessionId}-session-${deliveryIndex}`;
		return [
			client,
			{
				grantId: `grant-${rootIndex}-${deliveryIndex}`,
				token: `token-${rootIndex}-${deliveryIndex}`,
				expiresAt: new Date(Date.now() + 10_000).toISOString(),
				purpose: "agent_message",
				workerId: `worker-${rootIndex}`,
				workerInstanceId: `instance-${rootIndex}`,
				workerProcessStartId: "proc:test",
				socketIdentity: { dev: 10, ino: 20 },
				rootActiveSessionId,
				activeSessionId: targetActiveSessionId,
				targetSessionId,
				issuerGeneration: "supervisor-1",
				sender: {
					activeSessionId: `${rootActiveSessionId}-sender-${deliveryIndex}`,
					sessionId: `${rootActiveSessionId}-sender-session-${deliveryIndex}`,
					clientId: `worker:${rootIndex}:${deliveryIndex}`,
				},
			} satisfies DaemonWorkerPeerGrant,
		] as const;
	});
	const sessions = new Map(
		grants.map(([, grant]) => [
			grant.activeSessionId,
			{ runtime: { session: { sessionId: grant.targetSessionId } } },
		]),
	);
	const sendAgentSessionMessage = vi.fn(async (request: AgentMessageRequest) => {
		const grant = grants
			.map(([, currentGrant]) => currentGrant)
			.find((currentGrant) => currentGrant.activeSessionId === request.targetSelector);
		if (!grant) throw new Error(`Unknown target: ${request.targetSelector}`);
		const recordDelivery = () => {
			deliveries.push({
				id: `${grant.rootActiveSessionId}:${grant.activeSessionId}`,
				message: request.message,
				rootActiveSessionId: grant.rootActiveSessionId,
				targetActiveSessionId: grant.activeSessionId,
				targetSessionId: grant.targetSessionId,
				senderActiveSessionId: request.sender.activeSessionId ?? "",
				senderSessionId: request.sender.sessionId ?? "",
			});
			return { deliveryStatus: "queued" as const };
		};
		if (stalled && grant.activeSessionId.endsWith("target-0")) return stalledDelivery.promise.then(recordDelivery);
		return recordDelivery();
	});
	const daemon = Object.assign(Object.create(AgentDaemon.prototype), {
		options: { worker: { workerId: `worker-${rootIndex}`, workerInstanceId: `instance-${rootIndex}` } },
		promptAdmissions: new Map(),
		peerAdmissionsFenced: false,
		peerClaims: new Map(grants),
		supervisorClaims: new Map(),
		sessions,
		mutationDrain: { begin: vi.fn(), end: vi.fn() },
		sendAgentSessionMessage,
	}) as { handleLine(client: DaemonSocketClient, line: string): Promise<void> };

	return { daemon, clients, deliveries, stalledDelivery };
}

describe("direct worker transport fan-out", () => {
	it("keeps 39 routed sessions and production cached listing responsive while one direct get_state stalls", async () => {
		const { controlPlane, releaseRefresh } = createCachedListControlPlane();
		const workers = Array.from({ length: 40 }, () => new FakeDirectWorker());
		const clients = workers.map((worker) => routedClient(controlPlane, worker));
		const stalledWorker = workers[0]!;
		const stalledClient = clients[0]!;
		const healthyWorkers = workers.slice(1);
		const healthyClients = clients.slice(1);
		let stalledRequestSettled = false;
		const stalledRequest = stalledClient.request({ type: "get_state", activeSessionId: "active-0" }).finally(() => {
			stalledRequestSettled = true;
		});
		const cachedListRequest = controlPlane.request({ type: "list", refresh: false });

		try {
			const healthyRequests = healthyClients.map((client, index) =>
				client.request({ type: "get_state", activeSessionId: `active-${index + 1}` }),
			);
			for (const worker of healthyWorkers) worker.state.resolve(success("get_state"));

			const results = await Promise.all(healthyRequests);
			expect(results).toHaveLength(39);
			expect(results.every((result) => result.success)).toBe(true);
			await expect(cachedListRequest).resolves.toMatchObject({
				success: true,
				data: { sessions: [expect.objectContaining({ id: "cached-active" })] },
			});
			expect(stalledRequestSettled).toBe(false);
		} finally {
			stalledWorker.state.resolve(success("get_state"));
			releaseRefresh();
			await stalledRequest;
			for (const client of clients) client.close();
		}
	});

	it("keeps 63 production authorized agent-message deliveries and cached listing responsive while one target stalls", async () => {
		const { controlPlane, releaseRefresh } = createCachedListControlPlane();
		const fixtures = Array.from({ length: 16 }, (_, rootIndex) =>
			createAgentDaemonFixture(rootIndex, rootIndex === 0),
		);
		const deliveries = fixtures.flatMap((fixture, rootIndex) =>
			fixture.clients.map((client, deliveryIndex) => ({
				fixture,
				client,
				id: `root-${rootIndex}:root-${rootIndex}-target-${deliveryIndex}`,
				rootActiveSessionId: `root-${rootIndex}`,
				targetActiveSessionId: `root-${rootIndex}-target-${deliveryIndex}`,
				targetSessionId: `root-${rootIndex}-session-${deliveryIndex}`,
				senderActiveSessionId: `root-${rootIndex}-sender-${deliveryIndex}`,
				senderSessionId: `root-${rootIndex}-sender-session-${deliveryIndex}`,
			})),
		);
		const stalled = deliveries[0]!;
		let stalledDeliverySettled = false;
		const stalledDelivery = stalled.fixture.daemon
			.handleLine(
				stalled.client,
				JSON.stringify({ id: stalled.id, type: "peer_deliver_message", message: "authorized" }),
			)
			.finally(() => {
				stalledDeliverySettled = true;
			});
		const cachedListRequest = controlPlane.request({ type: "list", refresh: false });

		try {
			const completed = deliveries.slice(1);
			await Promise.all(
				completed.map(({ fixture, client, id }) =>
					fixture.daemon.handleLine(
						client,
						JSON.stringify({ id, type: "peer_deliver_message", message: "authorized" }),
					),
				),
			);
			const completedDeliveries = fixtures.flatMap((fixture) => fixture.deliveries);
			expect(completedDeliveries).toHaveLength(63);
			expect(new Set(completedDeliveries.map((delivery) => delivery.id)).size).toBe(63);
			for (const delivery of completedDeliveries) {
				expect(delivery).toMatchObject({
					rootActiveSessionId: delivery.targetActiveSessionId.split("-target-")[0],
					targetSessionId: delivery.targetActiveSessionId.replace("-target-", "-session-"),
					senderActiveSessionId: delivery.targetActiveSessionId.replace("-target-", "-sender-"),
					senderSessionId: delivery.targetActiveSessionId.replace("-target-", "-sender-session-"),
				});
			}
			await expect(cachedListRequest).resolves.toMatchObject({ success: true });
			expect(stalledDeliverySettled).toBe(false);
		} finally {
			stalled.fixture.stalledDelivery.resolve({ deliveryStatus: "queued" });
			releaseRefresh();
			await stalledDelivery;
		}
	});
});
