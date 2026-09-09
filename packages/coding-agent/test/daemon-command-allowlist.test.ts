import { describe, expect, it } from "vitest";
import { DAEMON_COMMAND_TYPES as WORKER_COMMAND_TYPES } from "../src/modes/daemon/daemon-mode.js";
import { DAEMON_COMMAND_COMPATIBILITY } from "../src/modes/daemon/daemon-protocol.js";
import { DAEMON_COMMAND_TYPES as SUPERVISOR_COMMAND_TYPES } from "../src/modes/daemon/daemon-supervisor.js";

const SUPERVISOR_ONLY_COMMANDS = new Set([
	"complete_owned_session",
	"get_direct_worker_transport",
	"list_agent_peers",
	"promote_owned_session",
	"reattach",
	"roster_subscribe",
	"roster_unsubscribe",
]);

describe("daemon command parser allowlists", () => {
	it("matches compatibility metadata except for supervisor-only commands", () => {
		const compatibilityCommands = new Set(Object.keys(DAEMON_COMMAND_COMPATIBILITY));
		const worker = new Set(WORKER_COMMAND_TYPES);
		const supervisor = new Set(SUPERVISOR_COMMAND_TYPES);

		expect(supervisor).toEqual(compatibilityCommands);
		expect(worker).toEqual(
			new Set([...compatibilityCommands].filter((command) => !SUPERVISOR_ONLY_COMMANDS.has(command))),
		);
		expect(new Set([...supervisor].filter((command) => !worker.has(command)))).toEqual(SUPERVISOR_ONLY_COMMANDS);
		expect(new Set([...worker].filter((command) => !supervisor.has(command)))).toEqual(new Set());
	});
});
