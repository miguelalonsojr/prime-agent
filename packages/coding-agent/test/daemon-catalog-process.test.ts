import { describe, expect, it } from "vitest";
import type { SessionInfo } from "../src/core/session-manager.js";
import {
	createDaemonCatalogSubprocessEnv,
	resolveCatalogSessionMatch,
} from "../src/modes/daemon/daemon-catalog-process.js";

function session(id: string, name: string | undefined, path: string): SessionInfo {
	return {
		id,
		name,
		path,
		cwd: "/tmp/project",
		rlmDepth: 0,
		created: new Date(0),
		modified: new Date(0),
		messageCount: 0,
		firstMessage: "",
		allMessagesText: "",
	};
}

describe("daemon catalog selector resolution", () => {
	it("enables node:sqlite for Node versions that still require the experimental flag", () => {
		expect(createDaemonCatalogSubprocessEnv({ NODE_OPTIONS: "--trace-warnings" }, "22.8.0", undefined)).toMatchObject(
			{
				NODE_OPTIONS: "--trace-warnings --experimental-sqlite",
			},
		);
		expect(createDaemonCatalogSubprocessEnv({}, "22.13.0", undefined).NODE_OPTIONS).toBeUndefined();
		expect(createDaemonCatalogSubprocessEnv({}, "22.8.0", "1.2.0").NODE_OPTIONS).toBeUndefined();
	});

	it("treats an exact name colliding with another session id prefix as ambiguous", () => {
		const sessions = [
			session("named-session-id", "target", "/tmp/by-name.jsonl"),
			session("target-prefix-id", "other", "/tmp/by-prefix.jsonl"),
		];

		expect(() => resolveCatalogSessionMatch(sessions, "target")).toThrow('Ambiguous session selector "target"');
	});
});
