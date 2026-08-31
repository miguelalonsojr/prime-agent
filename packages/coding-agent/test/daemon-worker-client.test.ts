import { mkdtempSync, rmSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
	DAEMON_PROTOCOL_NAME,
	DAEMON_PROTOCOL_VERSION,
	DAEMON_SCHEMA_ID,
	DAEMON_SCHEMA_REVISION,
	type DaemonPeerTransportTicket,
} from "../src/modes/daemon/daemon-protocol.js";
import { DaemonWorkerClient } from "../src/modes/daemon/daemon-worker-client.js";
import { type DaemonWorkerFrameHeader, isDaemonWorkerFrameHeader } from "../src/modes/daemon/daemon-worker-protocol.js";
import { PrivateFramedChannel } from "../src/modes/session-worker/private-framing.js";

const cleanup: Array<() => Promise<void>> = [];

afterEach(async () => {
	for (const close of cleanup.splice(0)) await close();
});

function ticket(socketPath: string): DaemonPeerTransportTicket {
	return {
		purpose: "session_client",
		socketPath,
		socketIdentity: { dev: 1, ino: 1 },
		workerId: "worker-1",
		workerInstanceId: "instance-1",
		rootActiveSessionId: "root-1",
		activeSessionId: "root-1",
		workerPid: process.pid,
		workerProcessStartId: "start-1",
		grantId: "grant-1",
		token: "peer-token",
		expiresAt: new Date(Date.now() + 10_000).toISOString(),
	};
}

async function createWorkerFixture(
	onCommand: (
		channel: PrivateFramedChannel<DaemonWorkerFrameHeader>,
		header: Extract<DaemonWorkerFrameHeader, { kind: "command" }>,
		payload: Record<string, unknown>,
	) => Promise<void>,
): Promise<{ socketPath: string; channels: PrivateFramedChannel<DaemonWorkerFrameHeader>[] }> {
	const directory = mkdtempSync(join(tmpdir(), "prime-worker-client-test-"));
	const socketPath = join(directory, "worker.sock");
	const channels: PrivateFramedChannel<DaemonWorkerFrameHeader>[] = [];
	const server = createServer((socket) => {
		const channel = new PrivateFramedChannel(socket, isDaemonWorkerFrameHeader);
		channels.push(channel);
		void channel.send(
			{ kind: "outbound", outboundType: "daemon_hello", payloadEncoding: "jsonl" },
			Buffer.from(
				JSON.stringify({
					type: "daemon_hello",
					protocol: {
						name: DAEMON_PROTOCOL_NAME,
						version: DAEMON_PROTOCOL_VERSION,
						schemaRevision: DAEMON_SCHEMA_REVISION,
						schemaId: DAEMON_SCHEMA_ID,
					},
					serverCapabilities: ["direct_peer_transport"],
				}),
			),
		);
		channel.onFrame((frame) => {
			if (frame.header.kind !== "command") return;
			const payload = JSON.parse(frame.payload.toString("utf8")) as Record<string, unknown>;
			void onCommand(channel, frame.header, payload);
		});
	});
	await new Promise<void>((resolveListen) => server.listen(socketPath, resolveListen));
	cleanup.push(async () => {
		for (const channel of channels) channel.close();
		await new Promise<void>((resolveClose) => server.close(() => resolveClose()));
		rmSync(directory, { recursive: true, force: true });
	});
	return { socketPath, channels };
}

describe("daemon worker direct client", () => {
	it("rejects a peer authentication response whose payload does not match its private frame", async () => {
		const fixture = await createWorkerFixture(async (channel, header) => {
			await channel.send(
				{
					kind: "outbound",
					outboundType: "response",
					requestId: header.requestId,
					payloadEncoding: "jsonl",
				},
				Buffer.from(
					JSON.stringify({
						id: "wrong-request",
						type: "response",
						command: "peer_auth",
						success: true,
						data: { workerId: "worker-1", workerInstanceId: "instance-1", purpose: "session_client" },
					}),
				),
			);
		});
		const client = new DaemonWorkerClient(fixture.socketPath);
		await client.connect();
		await client.waitForHello();

		await expect(client.authenticatePeer(ticket(fixture.socketPath))).rejects.toThrow(
			"Daemon worker response did not match its private frame",
		);
		client.close();
	});

	it("marks a peer request uncertain before a socket write callback reports failure", async () => {
		const client = new DaemonWorkerClient("/unused-worker.sock");
		const onAdmitted = vi.fn();
		Object.assign(client, {
			socket: { destroyed: false, destroy: vi.fn() },
			channel: {
				close: vi.fn(),
				send: async (_header: unknown, _payload: unknown, onWriteStarted?: () => void) => {
					onWriteStarted?.();
					throw new Error("write callback failed");
				},
			},
		});

		await expect(
			client.requestPeer({ type: "peer_deliver_message", message: "deliver once" }, 1000, onAdmitted),
		).rejects.toThrow("Daemon worker request outcome is uncertain after write began: peer_deliver_message");
		expect(onAdmitted).toHaveBeenCalledOnce();
		client.close();
	});

	it("keeps a peer request uncertain when the socket closes after write initiation", async () => {
		const client = new DaemonWorkerClient("/unused-worker.sock");
		Object.assign(client, {
			socket: { destroyed: false, destroy: vi.fn() },
			channel: {
				close: vi.fn(),
				send: async (_header: unknown, _payload: unknown, onWriteStarted?: () => void) => {
					onWriteStarted?.();
					(client as unknown as { rejectAll(error: Error): void }).rejectAll(new Error("worker socket closed"));
				},
			},
		});

		await expect(
			client.requestPeer({ type: "peer_deliver_message", message: "deliver once" }, 1000, vi.fn()),
		).rejects.toThrow("Daemon worker request outcome is uncertain after write began: peer_deliver_message");
		client.close();
	});

	it("keeps a malformed direct event local to that socket", async () => {
		const fixture = await createWorkerFixture(async (channel, header, payload) => {
			if (payload.type !== "peer_auth") return;
			await channel.send(
				{
					kind: "outbound",
					outboundType: "response",
					requestId: header.requestId,
					payloadEncoding: "jsonl",
				},
				Buffer.from(
					JSON.stringify({
						id: header.requestId,
						type: "response",
						command: "peer_auth",
						success: true,
						data: { workerId: "worker-1", workerInstanceId: "instance-1", purpose: "session_client" },
					}),
				),
			);
		});
		const client = new DaemonWorkerClient(fixture.socketPath);
		await client.connect();
		await client.waitForHello();
		await client.authenticatePeer(ticket(fixture.socketPath));
		const closed = new Promise<Error>((resolveClose) => client.onClose(resolveClose));
		await fixture.channels[0]?.send(
			{
				kind: "outbound",
				outboundType: "session_event",
				activeSessionId: "root-1",
				sessionEventType: "message_start",
				payloadEncoding: "jsonl",
			},
			Buffer.from(
				JSON.stringify({
					type: "session_event",
					activeSessionId: "root-1",
					event: { type: "message_end" },
				}),
			),
		);

		await expect(closed).resolves.toMatchObject({
			message: "Direct worker frame event type did not match its payload",
		});
		expect(client.isConnected).toBe(false);

		const second = new DaemonWorkerClient(fixture.socketPath);
		await second.connect();
		await expect(second.waitForHello()).resolves.toMatchObject({ type: "daemon_hello" });
		second.close();
	});
});
