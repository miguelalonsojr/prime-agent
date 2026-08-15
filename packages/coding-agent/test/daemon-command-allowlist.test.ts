import { describe, expect, it } from "vitest";
import { DAEMON_COMMAND_TYPES as WORKER_COMMAND_TYPES } from "../src/modes/daemon/daemon-mode.js";
import { DAEMON_COMMAND_COMPATIBILITY } from "../src/modes/daemon/daemon-protocol.js";
import { DAEMON_COMMAND_TYPES as SUPERVISOR_COMMAND_TYPES } from "../src/modes/daemon/daemon-supervisor.js";

// Commands the supervisor answers itself; a worker never sees them.
const SUPERVISOR_ONLY_COMMANDS = new Set(["reattach", "complete_owned_session", "promote_owned_session"]);

describe("daemon command allowlists", () => {
	it("accepts every command in the compatibility map at the supervisor", () => {
		const missing = Object.keys(DAEMON_COMMAND_COMPATIBILITY).filter((name) => !SUPERVISOR_COMMAND_TYPES.has(name));
		expect(missing).toEqual([]);
	});

	it("accepts every non-supervisor-only command in the compatibility map at the worker", () => {
		const missing = Object.keys(DAEMON_COMMAND_COMPATIBILITY).filter(
			(name) => !SUPERVISOR_ONLY_COMMANDS.has(name) && !WORKER_COMMAND_TYPES.has(name),
		);
		expect(missing).toEqual([]);
	});
});
