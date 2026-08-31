import { appendFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { afterEach, describe, expect, it, vi } from "vitest";
import { readSessionInfo, type SessionHeader } from "../src/core/session-manager.js";
import { deleteCatalogSessionFile } from "../src/modes/daemon/daemon-catalog-process.js";
import { getSessionMetadataCatalogPath, SessionMetadataCatalog } from "../src/modes/daemon/session-metadata-catalog.js";

const temporaryDirectories = new Set<string>();

function createTemporarySessionDir(): string {
	const directory = mkdtempSync(join(tmpdir(), "prime-session-catalog-"));
	temporaryDirectories.add(directory);
	return directory;
}

function writeSession(path: string, id: string, cwd: string, message: string): void {
	const timestamp = "2026-01-01T00:00:00.000Z";
	const header: SessionHeader = {
		type: "session",
		version: 3,
		id,
		timestamp,
		cwd,
	};
	const messageEntry = {
		type: "message",
		id: `${id}-message`,
		parentId: null,
		timestamp,
		message: {
			role: "user",
			content: [{ type: "text", text: message }],
			timestamp: Date.parse(timestamp),
		},
	};
	writeFileSync(path, `${JSON.stringify(header)}\n${JSON.stringify(messageEntry)}\n`);
}

function writeEmptySession(path: string, id: string, cwd: string): void {
	const timestamp = "2026-01-01T00:00:00.000Z";
	const header: SessionHeader = { type: "session", version: 3, id, timestamp, cwd };
	writeFileSync(path, `${JSON.stringify(header)}\n`);
}

function appendName(path: string, id: string, name: string): void {
	appendFileSync(
		path,
		`${JSON.stringify({
			type: "session_info",
			id: `${id}-name`,
			parentId: `${id}-message`,
			timestamp: "2026-01-02T00:00:00.000Z",
			name,
		})}\n`,
	);
}

afterEach(() => {
	for (const directory of temporaryDirectories) {
		rmSync(directory, { recursive: true, force: true });
	}
	temporaryDirectories.clear();
	vi.restoreAllMocks();
});

describe("session metadata catalog", () => {
	it.each(["missing", "empty"] as const)(
		"automatically populates a %s catalog without rewriting JSONL",
		async (kind) => {
			const sessionDir = createTemporarySessionDir();
			const sessionPath = join(sessionDir, "session-one.jsonl");
			writeSession(sessionPath, "session-one", "/tmp/project", "hello");
			const originalTranscript = readFileSync(sessionPath);
			const databasePath = getSessionMetadataCatalogPath(sessionDir);
			if (kind === "empty") {
				writeFileSync(databasePath, "");
			}
			const readInfo = vi.fn(readSessionInfo);
			const catalog = new SessionMetadataCatalog({ readSessionInfo: readInfo });

			try {
				const sessions = await catalog.warm(sessionDir);

				expect(sessions.map((session) => session.id)).toEqual(["session-one"]);
				expect(sessions[0]?.firstMessage).toBe("hello");
				expect(existsSync(databasePath)).toBe(true);
				expect(readInfo).toHaveBeenCalledTimes(1);
				expect(readFileSync(sessionPath)).toEqual(originalTranscript);

				await catalog.list(undefined, sessionDir);
				expect(readInfo).toHaveBeenCalledTimes(1);
				expect(readFileSync(sessionPath)).toEqual(originalTranscript);
			} finally {
				catalog.close();
			}
		},
	);

	it("repopulates an unpopulated catalog table on the next list", async () => {
		const sessionDir = createTemporarySessionDir();
		const sessionPath = join(sessionDir, "session-one.jsonl");
		writeSession(sessionPath, "session-one", "/tmp/project", "hello");
		const readInfo = vi.fn(readSessionInfo);
		const catalog = new SessionMetadataCatalog({ readSessionInfo: readInfo });

		try {
			await catalog.list(undefined, sessionDir);
			const database = new DatabaseSync(getSessionMetadataCatalogPath(sessionDir));
			database.exec("DELETE FROM sessions");
			database.close();

			const sessions = await catalog.list(undefined, sessionDir);

			expect(sessions.map((session) => session.id)).toEqual(["session-one"]);
			expect(readInfo).toHaveBeenCalledTimes(2);
		} finally {
			catalog.close();
		}
	});

	it("reconciles changed, new, and deleted transcript files", async () => {
		const sessionDir = createTemporarySessionDir();
		const firstPath = join(sessionDir, "session-one.jsonl");
		const secondPath = join(sessionDir, "session-two.jsonl");
		writeSession(firstPath, "session-one", "/tmp/project", "one");
		const readInfo = vi.fn(readSessionInfo);
		const catalog = new SessionMetadataCatalog({ readSessionInfo: readInfo });

		try {
			await catalog.list(undefined, sessionDir);
			appendName(firstPath, "session-one", "Renamed");
			writeSession(secondPath, "session-two", "/tmp/other", "two");

			const updated = await catalog.list(undefined, sessionDir);
			expect(updated.map((session) => [session.id, session.name])).toEqual([
				["session-one", "Renamed"],
				["session-two", undefined],
			]);
			expect(readInfo).toHaveBeenCalledTimes(3);

			rmSync(firstPath);
			const remaining = await catalog.list(undefined, sessionDir);
			expect(remaining.map((session) => session.id)).toEqual(["session-two"]);

			const database = new DatabaseSync(getSessionMetadataCatalogPath(sessionDir));
			const row = database.prepare("SELECT COUNT(*) AS count FROM sessions").get() as { count: number };
			database.close();
			expect(row.count).toBe(1);
		} finally {
			catalog.close();
		}
	});

	it("removes cached plaintext metadata as part of an explicit saved-session delete", async () => {
		const root = createTemporarySessionDir();
		const sessionDir = join(root, "sessions");
		mkdirSync(sessionDir, { recursive: true });
		const sessionPath = join(sessionDir, "session-one.jsonl");
		writeSession(sessionPath, "session-one", "/tmp/project", "private transcript text");
		const databasePath = getSessionMetadataCatalogPath(sessionDir);
		const catalog = new SessionMetadataCatalog();

		try {
			await catalog.list(undefined, sessionDir);
			const result = await deleteCatalogSessionFile(sessionPath, catalog);

			expect(result.ok).toBe(true);
			expect(existsSync(sessionPath)).toBe(false);
			const database = new DatabaseSync(databasePath);
			const row = database.prepare("SELECT COUNT(*) AS count FROM sessions").get() as { count: number };
			database.close();
			expect(row.count).toBe(0);
			catalog.close();
			expect(readFileSync(databasePath).includes(Buffer.from("private transcript text"))).toBe(false);
			for (const suffix of ["-wal", "-shm"]) {
				const sidecarPath = `${databasePath}${suffix}`;
				if (existsSync(sidecarPath)) {
					expect(readFileSync(sidecarPath).includes(Buffer.from("private transcript text"))).toBe(false);
				}
			}
		} finally {
			catalog.close();
		}
	});

	it("removes an empty inactive session from catalog metadata after deletion", async () => {
		const sessionDir = createTemporarySessionDir();
		const sessionPath = join(sessionDir, "empty-session.jsonl");
		writeEmptySession(sessionPath, "empty-session", "/tmp/project");
		const catalog = new SessionMetadataCatalog();

		try {
			expect((await catalog.list(undefined, sessionDir)).map((session) => session.id)).toContain("empty-session");
			await expect(deleteCatalogSessionFile(sessionPath, catalog)).resolves.toMatchObject({ ok: true });

			const database = new DatabaseSync(getSessionMetadataCatalogPath(sessionDir));
			const row = database.prepare("SELECT COUNT(*) AS count FROM sessions").get() as { count: number };
			database.close();
			expect(row.count).toBe(0);
		} finally {
			catalog.close();
		}
	});

	it("does not mutate a catalog created by a newer schema before falling back", async () => {
		const sessionDir = createTemporarySessionDir();
		const sessionPath = join(sessionDir, "session-one.jsonl");
		writeSession(sessionPath, "session-one", "/tmp/project", "hello");
		const databasePath = getSessionMetadataCatalogPath(sessionDir);
		const database = new DatabaseSync(databasePath);
		database.exec("PRAGMA user_version = 99");
		database.close();
		const originalDatabase = readFileSync(databasePath);
		const diagnostics: string[] = [];
		const catalog = new SessionMetadataCatalog({ onDiagnostic: (message) => diagnostics.push(message) });

		try {
			const sessions = await catalog.list(undefined, sessionDir);

			expect(sessions.map((session) => session.id)).toEqual(["session-one"]);
			expect(diagnostics[0]).toContain("schema 99 is newer");
		} finally {
			catalog.close();
		}
		expect(readFileSync(databasePath)).toEqual(originalDatabase);
		expect(existsSync(`${databasePath}-wal`)).toBe(false);
		expect(existsSync(`${databasePath}-shm`)).toBe(false);
	});

	it("returns a fresh JSONL scan when a catalog write fails", async () => {
		const sessionDir = createTemporarySessionDir();
		const sessionPath = join(sessionDir, "session-one.jsonl");
		writeSession(sessionPath, "session-one", "/tmp/project", "hello");
		const firstCatalog = new SessionMetadataCatalog();
		await firstCatalog.list(undefined, sessionDir);
		firstCatalog.close();

		const database = new DatabaseSync(getSessionMetadataCatalogPath(sessionDir));
		database.exec(`
			CREATE TRIGGER block_session_update BEFORE UPDATE ON sessions
			BEGIN
				SELECT RAISE(FAIL, 'blocked catalog update');
			END;
		`);
		database.close();
		appendName(sessionPath, "session-one", "Fresh from JSONL");
		const diagnostics: string[] = [];
		const catalog = new SessionMetadataCatalog({ onDiagnostic: (message) => diagnostics.push(message) });

		try {
			const sessions = await catalog.list(undefined, sessionDir);

			expect(sessions[0]?.name).toBe("Fresh from JSONL");
			expect(diagnostics[0]).toContain("using JSONL scan");
		} finally {
			catalog.close();
		}
	});

	it("falls back to JSONL scanning when SQLite is corrupt", async () => {
		const sessionDir = createTemporarySessionDir();
		const sessionPath = join(sessionDir, "session-one.jsonl");
		writeSession(sessionPath, "session-one", "/tmp/project", "hello");
		const originalTranscript = readFileSync(sessionPath);
		writeFileSync(getSessionMetadataCatalogPath(sessionDir), "not a sqlite database");
		const diagnostics: string[] = [];
		const catalog = new SessionMetadataCatalog({ onDiagnostic: (message) => diagnostics.push(message) });

		try {
			const sessions = await catalog.list(undefined, sessionDir);

			expect(sessions.map((session) => session.id)).toEqual(["session-one"]);
			expect(diagnostics).toHaveLength(1);
			expect(diagnostics[0]).toContain("using JSONL scan");
			expect(readFileSync(sessionPath)).toEqual(originalTranscript);
		} finally {
			catalog.close();
		}
	});

	it("preserves cwd filtering and streamed item callbacks", async () => {
		const sessionDir = createTemporarySessionDir();
		mkdirSync(sessionDir, { recursive: true });
		writeSession(join(sessionDir, "project.jsonl"), "project", "/tmp/project", "project message");
		writeSession(join(sessionDir, "other.jsonl"), "other", "/tmp/other", "other message");
		const discovered: string[] = [];
		const catalog = new SessionMetadataCatalog();

		try {
			const sessions = await catalog.list("/tmp/project", sessionDir, {
				onSession: (session) => discovered.push(session.id),
			});

			expect(sessions.map((session) => session.id)).toEqual(["project"]);
			expect(discovered).toEqual(["project"]);
		} finally {
			catalog.close();
		}
	});
});
