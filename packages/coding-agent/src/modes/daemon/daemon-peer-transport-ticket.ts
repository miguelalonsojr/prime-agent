import type { DaemonPeerTransportPurpose, DaemonPeerTransportTicket } from "./daemon-protocol.js";

export function parseDaemonPeerTransportTicket(
	value: unknown,
	purpose: DaemonPeerTransportPurpose,
): DaemonPeerTransportTicket | undefined {
	if (!value || typeof value !== "object") return undefined;
	const candidate = value as Partial<DaemonPeerTransportTicket>;
	if (
		candidate.purpose !== purpose ||
		typeof candidate.socketPath !== "string" ||
		typeof candidate.workerId !== "string" ||
		typeof candidate.workerInstanceId !== "string" ||
		typeof candidate.rootActiveSessionId !== "string" ||
		typeof candidate.activeSessionId !== "string" ||
		typeof candidate.workerPid !== "number" ||
		typeof candidate.workerProcessStartId !== "string" ||
		typeof candidate.grantId !== "string" ||
		typeof candidate.token !== "string" ||
		typeof candidate.expiresAt !== "string"
	) {
		return undefined;
	}
	if (
		!candidate.socketIdentity ||
		typeof candidate.socketIdentity.dev !== "number" ||
		typeof candidate.socketIdentity.ino !== "number"
	) {
		return undefined;
	}
	return candidate as DaemonPeerTransportTicket;
}
