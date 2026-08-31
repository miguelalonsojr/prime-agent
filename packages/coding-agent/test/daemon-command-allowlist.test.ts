import { describe, expect, it } from "vitest";
import { DAEMON_COMMAND_TYPES as WORKER_COMMAND_TYPES } from "../src/modes/daemon/daemon-mode.js";
import { DAEMON_COMMAND_COMPATIBILITY } from "../src/modes/daemon/daemon-protocol.js";
import { DAEMON_COMMAND_TYPES as SUPERVISOR_COMMAND_TYPES } from "../src/modes/daemon/daemon-supervisor.js";
import { isDirectSessionCommand } from "../src/modes/daemon/daemon-worker-protocol.js";

// Commands the supervisor answers itself; a worker never sees them.
const SUPERVISOR_ONLY_COMMANDS = new Set([
	"reattach",
	"complete_owned_session",
	"promote_owned_session",
	"list_agent_peers",
	"get_direct_worker_transport",
	"get_agent_message_transport",
]);

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

	it.each(["set_kernel_cwd", "set_model", "set_scoped_models", "set_thinking_level"] as const)(
		"routes %s directly to the owning worker",
		(type) => {
			expect(isDirectSessionCommand({ type })).toBe(true);
		},
	);

	it.each(["list", "list_agent_peers", "get_direct_worker_transport", "get_agent_message_transport"] as const)(
		"keeps %s on the supervisor control plane",
		(type) => {
			expect(isDirectSessionCommand({ type })).toBe(false);
		},
	);
});
