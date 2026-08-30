import { chmodSync, existsSync } from "node:fs";
import { readdir, stat } from "node:fs/promises";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import type { DatabaseSync as NodeDatabaseSync } from "node:sqlite";
import { getSessionsDir } from "../../config.js";
import {
	getDefaultSessionDir,
	readSessionInfo,
	type SessionInfo,
	type SessionListCallbacks,
	SessionManager,
} from "../../core/session-manager.js";

const CATALOG_SCHEMA_VERSION = 1;
const CATALOG_FILENAME = ".session-metadata.sqlite";
const CATALOG_BUSY_TIMEOUT_MS = 5000;
const WRITE_BATCH_SIZE = 100;

const require = createRequire(import.meta.url);

type DatabaseSyncConstructor = new (location: string) => NodeDatabaseSync;

function loadDatabaseSync(): DatabaseSyncConstructor {
	try {
		const sqlite = require("node:sqlite") as { DatabaseSync?: DatabaseSyncConstructor };
		if (sqlite.DatabaseSync) {
			return sqlite.DatabaseSync;
		}
	} catch {
		// Node 22.8-22.12 requires --experimental-sqlite; callers fall back to JSONL.
	}
	throw new Error("node:sqlite is unavailable");
}

interface SessionMetadataRow {
	path: string;
	file_size: number;
	file_mtime_ms: number;
	id: string;
	cwd: string;
	name: string | null;
	state_status: string | null;
	parent_session_path: string | null;
	rlm_depth: number;
	created_ms: number;
	modified_ms: number;
	message_count: number;
	first_message: string;
	search_text: string;
	agent_status_summary: string | null;
	agent_status_task_state: string | null;
	agent_status_message_count: number | null;
}

interface CatalogUpdate {
	info: SessionInfo;
	fileSize: number;
	fileMtimeMs: number;
}

export interface SessionMetadataCatalogOptions {
	databasePathForSessionDir?: (sessionDir: string) => string;
	readSessionInfo?: (path: string) => Promise<SessionInfo | null>;
	onDiagnostic?: (message: string) => void;
}

export function getSessionMetadataCatalogPath(sessionDir: string): string {
	return join(resolve(sessionDir), CATALOG_FILENAME);
}

function restrictCatalogFilePermissions(databasePath: string): void {
	for (const path of [databasePath, `${databasePath}-wal`, `${databasePath}-shm`]) {
		try {
			chmodSync(path, 0o600);
		} catch {
			// The containing sessions directory already protects cached session metadata.
		}
	}
}

function stateFromRow(status: string | null): SessionInfo["state"] {
	if (status === "active" || status === "archived" || status === "crash") {
		return { status };
	}
	return undefined;
}

function agentStatusFromRow(row: SessionMetadataRow): SessionInfo["agentStatus"] {
	if (row.agent_status_summary === null || row.agent_status_message_count === null) {
		return undefined;
	}
	const taskState =
		row.agent_status_task_state === "needs_input" || row.agent_status_task_state === "completed"
			? row.agent_status_task_state
			: undefined;
	return {
		summary: row.agent_status_summary,
		...(taskState ? { taskState } : {}),
		basedOnMessageCount: row.agent_status_message_count,
	};
}

function sessionInfoFromRow(row: SessionMetadataRow): SessionInfo {
	const created = new Date(row.created_ms);
	const modified = new Date(row.modified_ms);
	if (
		typeof row.path !== "string" ||
		typeof row.id !== "string" ||
		typeof row.cwd !== "string" ||
		(row.name !== null && typeof row.name !== "string") ||
		(row.parent_session_path !== null && typeof row.parent_session_path !== "string") ||
		!Number.isInteger(row.rlm_depth) ||
		Number.isNaN(created.getTime()) ||
		Number.isNaN(modified.getTime()) ||
		!Number.isInteger(row.message_count) ||
		typeof row.first_message !== "string" ||
		typeof row.search_text !== "string"
	) {
		throw new Error("Invalid session metadata catalog row");
	}
	const state = stateFromRow(row.state_status);
	const agentStatus = agentStatusFromRow(row);
	return {
		path: row.path,
		id: row.id,
		cwd: row.cwd,
		...(row.name === null ? {} : { name: row.name }),
		...(state ? { state } : {}),
		...(row.parent_session_path === null ? {} : { parentSessionPath: row.parent_session_path }),
		rlmDepth: row.rlm_depth,
		created,
		modified,
		messageCount: row.message_count,
		firstMessage: row.first_message,
		allMessagesText: row.search_text,
		...(agentStatus ? { agentStatus } : {}),
	};
}

function sessionMatchesCwd(session: SessionInfo, cwd: string): boolean {
	return session.cwd.length > 0 && resolve(session.cwd) === resolve(cwd);
}

function sortSessions(sessions: SessionInfo[]): SessionInfo[] {
	return sessions.sort((left, right) => right.modified.getTime() - left.modified.getTime());
}

class SessionMetadataStore {
	private database?: NodeDatabaseSync;
	private disabled = false;
	private reconciliation?: Promise<SessionInfo[]>;

	constructor(
		private readonly sessionDir: string,
		private readonly databasePath: string,
		private readonly readInfo: (path: string) => Promise<SessionInfo | null>,
		private readonly onDiagnostic: (message: string) => void,
	) {}

	async list(callbacks?: SessionListCallbacks): Promise<SessionInfo[]> {
		if (this.reconciliation) {
			const sessions = await this.reconciliation;
			this.emitCachedResult(sessions, callbacks);
			return sessions;
		}

		const reconciliation = this.reconcile(callbacks);
		this.reconciliation = reconciliation;
		try {
			return await reconciliation;
		} finally {
			if (this.reconciliation === reconciliation) {
				this.reconciliation = undefined;
			}
		}
	}

	async invalidate(sessionPath: string): Promise<void> {
		if (this.reconciliation) {
			await this.reconciliation.catch(() => undefined);
		}
		if (this.disabled) {
			return;
		}
		try {
			const database = this.openDatabase();
			database.prepare("DELETE FROM sessions WHERE path = ?").run(resolve(sessionPath));
			database.exec("PRAGMA wal_checkpoint(TRUNCATE)");
			restrictCatalogFilePermissions(this.databasePath);
		} catch (error) {
			this.disable(error);
		}
	}

	close(): void {
		try {
			this.database?.close();
		} catch {
			// The catalog is derived; close failures must not affect session shutdown.
		}
		this.database = undefined;
	}

	private async reconcile(callbacks?: SessionListCallbacks): Promise<SessionInfo[]> {
		if (!existsSync(this.sessionDir)) {
			return [];
		}
		if (this.disabled) {
			return SessionManager.listAll(callbacks, this.sessionDir);
		}

		let database: NodeDatabaseSync;
		let rows: SessionMetadataRow[];
		try {
			database = this.openDatabase();
			rows = database.prepare("SELECT * FROM sessions").all() as unknown as SessionMetadataRow[];
		} catch (error) {
			this.disable(error);
			return SessionManager.listAll(callbacks, this.sessionDir);
		}

		let filenames: string[];
		try {
			filenames = (await readdir(this.sessionDir)).filter((name) => name.endsWith(".jsonl"));
		} catch (error) {
			this.disable(error);
			return SessionManager.listAll(callbacks, this.sessionDir);
		}

		const rowsByPath = new Map(rows.map((row) => [row.path, row]));
		const presentPaths = new Set<string>();
		const updates: CatalogUpdate[] = [];
		const invalidPaths: string[] = [];
		const sessions: SessionInfo[] = [];
		let loaded = 0;

		for (const filename of filenames) {
			const path = join(this.sessionDir, filename);
			presentPaths.add(path);
			let info: SessionInfo | null = null;
			try {
				const fileStats = await stat(path);
				const row = rowsByPath.get(path);
				if (row && row.file_size === fileStats.size && row.file_mtime_ms === fileStats.mtimeMs) {
					try {
						info = sessionInfoFromRow(row);
					} catch {
						info = null;
					}
				}
				if (!info) {
					info = await this.readInfo(path);
					if (info) {
						updates.push({ info, fileSize: fileStats.size, fileMtimeMs: fileStats.mtimeMs });
					} else {
						invalidPaths.push(path);
					}
				}
			} catch {
				invalidPaths.push(path);
			}
			loaded++;
			callbacks?.onProgress?.(loaded, filenames.length);
			if (info) {
				sessions.push(info);
				callbacks?.onSession?.(info);
			}
		}

		const deletedPaths = rows.filter((row) => !presentPaths.has(row.path)).map((row) => row.path);
		try {
			this.applyChanges(database, updates, [...invalidPaths, ...deletedPaths]);
		} catch (error) {
			this.disable(error);
			return SessionManager.listAll(undefined, this.sessionDir);
		}
		return sortSessions(sessions);
	}

	private openDatabase(): NodeDatabaseSync {
		if (this.database) {
			return this.database;
		}
		const DatabaseSync = loadDatabaseSync();
		const database = new DatabaseSync(this.databasePath);
		try {
			const versionRow = database.prepare("PRAGMA user_version").get() as { user_version?: number } | undefined;
			const version = versionRow?.user_version ?? 0;
			if (version > CATALOG_SCHEMA_VERSION) {
				throw new Error(`Session metadata catalog schema ${version} is newer than ${CATALOG_SCHEMA_VERSION}`);
			}
			restrictCatalogFilePermissions(this.databasePath);
			database.exec(`PRAGMA busy_timeout = ${CATALOG_BUSY_TIMEOUT_MS}`);
			database.exec("PRAGMA secure_delete = ON");
			database.exec("PRAGMA journal_mode = WAL");
			database.exec("PRAGMA synchronous = NORMAL");
			restrictCatalogFilePermissions(this.databasePath);
			if (version === 0) {
				database.exec("BEGIN IMMEDIATE");
				try {
					database.exec(`
						CREATE TABLE IF NOT EXISTS sessions (
							path TEXT PRIMARY KEY,
							file_size INTEGER NOT NULL,
							file_mtime_ms REAL NOT NULL,
							id TEXT NOT NULL,
							cwd TEXT NOT NULL,
							name TEXT,
							state_status TEXT,
							parent_session_path TEXT,
							rlm_depth INTEGER NOT NULL,
							created_ms REAL NOT NULL,
							modified_ms REAL NOT NULL,
							message_count INTEGER NOT NULL,
							first_message TEXT NOT NULL,
							search_text TEXT NOT NULL,
							agent_status_summary TEXT,
							agent_status_task_state TEXT,
							agent_status_message_count INTEGER
						);
						CREATE INDEX IF NOT EXISTS sessions_modified_idx ON sessions(modified_ms DESC);
						PRAGMA user_version = ${CATALOG_SCHEMA_VERSION};
					`);
					database.exec("COMMIT");
				} catch (error) {
					database.exec("ROLLBACK");
					throw error;
				}
			}
			restrictCatalogFilePermissions(this.databasePath);
			this.database = database;
			return database;
		} catch (error) {
			try {
				database.close();
			} catch {
				// Preserve the original open/schema error.
			}
			throw error;
		}
	}

	private applyChanges(database: NodeDatabaseSync, updates: CatalogUpdate[], deletedPaths: string[]): void {
		const upsert = database.prepare(`
			INSERT INTO sessions (
				path, file_size, file_mtime_ms, id, cwd, name, state_status, parent_session_path,
				rlm_depth, created_ms, modified_ms, message_count, first_message, search_text,
				agent_status_summary, agent_status_task_state, agent_status_message_count
			) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
			ON CONFLICT(path) DO UPDATE SET
				file_size = excluded.file_size,
				file_mtime_ms = excluded.file_mtime_ms,
				id = excluded.id,
				cwd = excluded.cwd,
				name = excluded.name,
				state_status = excluded.state_status,
				parent_session_path = excluded.parent_session_path,
				rlm_depth = excluded.rlm_depth,
				created_ms = excluded.created_ms,
				modified_ms = excluded.modified_ms,
				message_count = excluded.message_count,
				first_message = excluded.first_message,
				search_text = excluded.search_text,
				agent_status_summary = excluded.agent_status_summary,
				agent_status_task_state = excluded.agent_status_task_state,
				agent_status_message_count = excluded.agent_status_message_count
		`);
		const remove = database.prepare("DELETE FROM sessions WHERE path = ?");
		const changes: Array<CatalogUpdate | string> = [...updates, ...new Set(deletedPaths)];

		for (let offset = 0; offset < changes.length; offset += WRITE_BATCH_SIZE) {
			database.exec("BEGIN IMMEDIATE");
			try {
				for (const change of changes.slice(offset, offset + WRITE_BATCH_SIZE)) {
					if (typeof change === "string") {
						remove.run(change);
						continue;
					}
					const { info, fileSize, fileMtimeMs } = change;
					upsert.run(
						info.path,
						fileSize,
						fileMtimeMs,
						info.id,
						info.cwd,
						info.name ?? null,
						info.state?.status ?? null,
						info.parentSessionPath ?? null,
						info.rlmDepth,
						info.created.getTime(),
						info.modified.getTime(),
						info.messageCount,
						info.firstMessage,
						info.allMessagesText,
						info.agentStatus?.summary ?? null,
						info.agentStatus?.taskState ?? null,
						info.agentStatus?.basedOnMessageCount ?? null,
					);
				}
				database.exec("COMMIT");
			} catch (error) {
				try {
					database.exec("ROLLBACK");
				} catch {
					// Preserve the write error.
				}
				throw error;
			}
		}
		restrictCatalogFilePermissions(this.databasePath);
	}

	private emitCachedResult(sessions: readonly SessionInfo[], callbacks?: SessionListCallbacks): void {
		callbacks?.onProgress?.(sessions.length, sessions.length);
		for (const session of sessions) {
			callbacks?.onSession?.(session);
		}
	}

	private disable(error: unknown): void {
		this.close();
		this.disabled = true;
		this.onDiagnostic(
			`Session metadata catalog unavailable at ${this.databasePath}; using JSONL scan: ${error instanceof Error ? error.message : String(error)}`,
		);
	}
}

export class SessionMetadataCatalog {
	private readonly stores = new Map<string, SessionMetadataStore>();
	private readonly databasePathForSessionDir: (sessionDir: string) => string;
	private readonly readInfo: (path: string) => Promise<SessionInfo | null>;
	private readonly onDiagnostic: (message: string) => void;

	constructor(options: SessionMetadataCatalogOptions = {}) {
		this.databasePathForSessionDir = options.databasePathForSessionDir ?? getSessionMetadataCatalogPath;
		this.readInfo = options.readSessionInfo ?? readSessionInfo;
		this.onDiagnostic = options.onDiagnostic ?? (() => {});
	}

	warm(sessionDir?: string): Promise<SessionInfo[]> {
		return this.list(undefined, sessionDir);
	}

	async list(cwd?: string, sessionDir?: string, callbacks?: SessionListCallbacks): Promise<SessionInfo[]> {
		const dir = resolve(sessionDir ?? (cwd ? getDefaultSessionDir(cwd) : getSessionsDir()));
		const matches = cwd ? (session: SessionInfo) => sessionMatchesCwd(session, cwd) : () => true;
		const storeCallbacks: SessionListCallbacks | undefined = callbacks
			? {
					onProgress: callbacks.onProgress,
					onSession: callbacks.onSession
						? (session) => matches(session) && callbacks.onSession?.(session)
						: undefined,
				}
			: undefined;
		const sessions = await this.store(dir).list(storeCallbacks);
		return cwd ? sessions.filter(matches) : sessions;
	}

	async invalidate(sessionPath: string): Promise<void> {
		const normalizedPath = resolve(sessionPath);
		const sessionDir = dirname(normalizedPath);
		if (!this.stores.has(sessionDir) && !existsSync(this.databasePathForSessionDir(sessionDir))) {
			return;
		}
		await this.store(sessionDir).invalidate(normalizedPath);
	}

	close(): void {
		for (const store of this.stores.values()) {
			store.close();
		}
		this.stores.clear();
	}

	private store(sessionDir: string): SessionMetadataStore {
		let store = this.stores.get(sessionDir);
		if (!store) {
			store = new SessionMetadataStore(
				sessionDir,
				this.databasePathForSessionDir(sessionDir),
				this.readInfo,
				this.onDiagnostic,
			);
			this.stores.set(sessionDir, store);
		}
		return store;
	}
}
