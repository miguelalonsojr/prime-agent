import { mkdtempSync, rmSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ActiveSessionState } from "../src/modes/daemon/active-session-state.js";
import { DaemonClient } from "../src/modes/daemon/daemon-client.js";
import { AgentDaemon } from "../src/modes/daemon/daemon-mode.js";
import type { DaemonPeerTransportTicket } from "../src/modes/daemon/daemon-protocol.js";
import { getDaemonSocketIdentity } from "../src/modes/daemon/daemon-socket.js";
import { DaemonWorkerClient } from "../src/modes/daemon/daemon-worker-client.js";
import { DAEMON_WORKER_SUPERVISOR_SOCKET_ENV } from "../src/modes/daemon/daemon-worker-protocol.js";

const cleanup: Array<() => Promise<void>> = [];

afterEach(async () => {
	vi.restoreAllMocks();
	for (const close of cleanup.splice(0)) await close();
});

describe("direct agent-message transport", () => {
	it("does not fall back after the direct socket write begins", async () => {
		const tempDir = mkdtempSync(join(tmpdir(), "pa-direct-msg-write-"));
		const socketPath = join(tempDir, "worker.sock");
		const server = createServer();
		await new Promise<void>((resolve) => server.listen(socketPath, resolve));
		cleanup.push(async () => {
			await new Promise<void>((resolve) => server.close(() => resolve()));
			rmSync(tempDir, { recursive: true, force: true });
		});
		const socketIdentity = getDaemonSocketIdentity(socketPath);
		if (!socketIdentity) throw new Error("Test requires a Unix socket identity");
		const ticket: DaemonPeerTransportTicket = {
			purpose: "agent_message",
			socketPath,
			socketIdentity,
			workerId: "target-worker",
			workerInstanceId: "target-instance",
			rootActiveSessionId: "target-root",
			activeSessionId: "target-active",
			workerPid: process.pid,
			workerProcessStartId: "proc:test",
			grantId: "grant-1",
			token: "peer-token",
			expiresAt: new Date(Date.now() + 10_000).toISOString(),
		};
		const previousSupervisorSocket = process.env[DAEMON_WORKER_SUPERVISOR_SOCKET_ENV];
		process.env[DAEMON_WORKER_SUPERVISOR_SOCKET_ENV] = join(tempDir, "supervisor.sock");
		cleanup.push(async () => {
			if (previousSupervisorSocket === undefined) delete process.env[DAEMON_WORKER_SUPERVISOR_SOCKET_ENV];
			else process.env[DAEMON_WORKER_SUPERVISOR_SOCKET_ENV] = previousSupervisorSocket;
		});
		vi.spyOn(DaemonClient.prototype, "connect").mockResolvedValue();
		vi.spyOn(DaemonClient.prototype, "waitForHello").mockResolvedValue({} as never);
		vi.spyOn(DaemonClient.prototype, "supportsServerCapability").mockReturnValue(true);
		const supervisorRequest = vi.spyOn(DaemonClient.prototype, "request").mockImplementation(async (command) => {
			if (command.type === "get_agent_message_transport") {
				return {
					type: "response",
					command: command.type,
					success: true,
					data: ticket,
				};
			}
			return {
				type: "response",
				command: command.type,
				success: true,
				data: { fallback: true },
			};
		});
		vi.spyOn(DaemonWorkerClient.prototype, "connect").mockImplementation(async function (this: DaemonWorkerClient) {
			Object.assign(this, {
				socket: { destroyed: false, destroy: vi.fn() },
				channel: {
					close: vi.fn(),
					send: async (_header: unknown, _payload: unknown, onWriteStarted?: () => void) => {
						onWriteStarted?.();
						throw new Error("write callback failed");
					},
				},
			});
		});
		vi.spyOn(DaemonWorkerClient.prototype, "waitForHello").mockResolvedValue({} as never);
		vi.spyOn(DaemonWorkerClient.prototype, "authenticatePeer").mockResolvedValue();
		const daemon = new AgentDaemon(join(tempDir, "source.sock"), {
			defaultSessionConfig: { agentDir: tempDir, cwd: tempDir },
			createRuntime: vi.fn(),
			worker: {
				authenticationToken: "source-token",
				workerInstanceId: "source-instance",
			},
		});
		const sendRemote = (
			daemon as unknown as {
				sendRemoteAgentSessionMessage(
					fromState: ActiveSessionState,
					targetSelector: string,
					message: string,
				): Promise<unknown>;
			}
		).sendRemoteAgentSessionMessage.bind(daemon);

		await expect(
			sendRemote({ activeSessionId: "source" } as ActiveSessionState, "target", "deliver once"),
		).rejects.toThrow("Daemon worker request outcome is uncertain after write began: peer_deliver_message");
		expect(supervisorRequest).toHaveBeenCalledOnce();
		expect(supervisorRequest).toHaveBeenCalledWith(
			expect.objectContaining({ type: "get_agent_message_transport" }),
			5000,
		);
	});
});
