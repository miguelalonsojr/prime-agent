import { describe, expect, it } from "vitest";
import type {
	DaemonClientCloseListener,
	DaemonClientMessageListener,
	DaemonClientRequestOptions,
	DaemonHello,
	DaemonTransportClient,
} from "../src/modes/daemon/daemon-client.js";
import type { DaemonCommand, DaemonResponse } from "../src/modes/daemon/daemon-protocol.js";
import { DaemonRoutedClient } from "../src/modes/daemon/daemon-routed-client.js";
import type { DaemonWorkerClient } from "../src/modes/daemon/daemon-worker-client.js";

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

class FakeSupervisor {
	readonly listRequests: DaemonCommand[] = [];
	readonly cachedList = new Deferred<DaemonResponse>();
	readonly hello: DaemonHello | undefined;
	readonly isConnected = true;

	async request(command: DaemonCommand): Promise<DaemonResponse> {
		this.listRequests.push(command);
		if (command.type === "list") return this.cachedList.promise;
		throw new Error(`Unexpected supervisor command: ${command.type}`);
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
}

class FakeAuthorizedAgentMessageTransport {
	readonly deliveries: AgentMessageDelivery[] = [];
	readonly pending = new Map<string, Deferred<void>>();

	deliver(delivery: AgentMessageDelivery): Promise<void> {
		if (this.pending.has(delivery.id)) throw new Error(`Duplicate delivery: ${delivery.id}`);
		const deferred = new Deferred<void>();
		this.pending.set(delivery.id, deferred);
		return deferred.promise.then(() => {
			this.deliveries.push(delivery);
		});
	}

	release(id: string): void {
		const deferred = this.pending.get(id);
		if (!deferred) throw new Error(`Unknown delivery: ${id}`);
		deferred.resolve();
	}
}

function success(command: DaemonCommand["type"]): DaemonResponse {
	return { type: "response", command, success: true } as DaemonResponse;
}

function routedClient(supervisor: FakeSupervisor, worker: FakeDirectWorker): DaemonRoutedClient {
	return new DaemonRoutedClient(
		supervisor as unknown as DaemonTransportClient,
		worker as unknown as DaemonWorkerClient,
	);
}

describe("direct worker transport fan-out", () => {
	it("keeps 39 routed sessions and cached listing responsive while one direct get_state stalls", async () => {
		const supervisor = new FakeSupervisor();
		const workers = Array.from({ length: 40 }, () => new FakeDirectWorker());
		const clients = workers.map((worker) => routedClient(supervisor, worker));
		const stalledWorker = workers[0]!;
		const stalledClient = clients[0]!;
		const healthyWorkers = workers.slice(1);
		const healthyClients = clients.slice(1);
		let stalledRequestSettled = false;
		const stalledRequest = stalledClient.request({ type: "get_state", activeSessionId: "active-0" }).finally(() => {
			stalledRequestSettled = true;
		});
		const cachedListRequest = supervisor.request({ type: "list", refresh: false });

		try {
			const healthyRequests = healthyClients.map((client, index) =>
				client.request({ type: "get_state", activeSessionId: `active-${index + 1}` }),
			);
			for (const worker of healthyWorkers) worker.state.resolve(success("get_state"));
			supervisor.cachedList.resolve(success("list"));

			const results = await Promise.all(healthyRequests);
			expect(results).toHaveLength(39);
			expect(results.every((result) => result.success)).toBe(true);
			await expect(cachedListRequest).resolves.toMatchObject({ success: true });
			expect(stalledRequestSettled).toBe(false);
		} finally {
			stalledWorker.state.resolve(success("get_state"));
			await stalledRequest;
			for (const client of clients) client.close();
		}
	});

	it("keeps 63 authorized agent-message deliveries and cached listing responsive while one target stalls", async () => {
		const supervisor = new FakeSupervisor();
		const transports = Array.from({ length: 16 }, () => new FakeAuthorizedAgentMessageTransport());
		const deliveries = transports.flatMap((transport, rootIndex) =>
			Array.from({ length: 4 }, (_, deliveryIndex) => ({
				transport,
				delivery: { id: `root-${rootIndex}-delivery-${deliveryIndex}`, message: "authorized delivery" },
			})),
		);
		const stalled = deliveries[0]!;
		let stalledDeliverySettled = false;
		const stalledDelivery = stalled.transport.deliver(stalled.delivery).finally(() => {
			stalledDeliverySettled = true;
		});
		const cachedListRequest = supervisor.request({ type: "list", refresh: false });

		try {
			const completed = deliveries.slice(1);
			const deliveryPromises = completed.map(({ transport, delivery }) => transport.deliver(delivery));
			for (const { transport, delivery } of completed) transport.release(delivery.id);
			supervisor.cachedList.resolve(success("list"));
			await Promise.all(deliveryPromises);

			const completedDeliveries = transports.flatMap((transport) => transport.deliveries);
			expect(completedDeliveries).toHaveLength(63);
			expect(new Set(completedDeliveries.map((delivery) => delivery.id)).size).toBe(63);
			await expect(cachedListRequest).resolves.toMatchObject({ success: true });
			expect(stalledDeliverySettled).toBe(false);
		} finally {
			stalled.transport.release(stalled.delivery.id);
			await stalledDelivery;
		}
	});
});
