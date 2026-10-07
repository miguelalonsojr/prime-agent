import { access, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { createEditTool } from "../src/core/tools/edit.js";
import { drainFileMutationQueue, withFileMutationQueue } from "../src/core/tools/file-mutation-queue.js";
import { createDeferred } from "./suite/scheduling.js";

function delay(ms: number): Promise<void> {
	return new Promise((resolve) => setTimeout(resolve, ms));
}

const tempDirs: string[] = [];

async function createTempDir(): Promise<string> {
	const dir = await mkdtemp(join(tmpdir(), "pi-file-mutation-queue-"));
	tempDirs.push(dir);
	return dir;
}

afterEach(async () => {
	await Promise.all(tempDirs.splice(0, tempDirs.length).map((dir) => rm(dir, { recursive: true, force: true })));
});

describe("withFileMutationQueue", () => {
	it("serializes operations for the same file", async () => {
		const order: string[] = [];
		const path = "/tmp/file-mutation-queue-same";

		const first = withFileMutationQueue(path, async () => {
			order.push("first:start");
			await delay(30);
			order.push("first:end");
		});
		const second = withFileMutationQueue(path, async () => {
			order.push("second:start");
			order.push("second:end");
		});

		await Promise.all([first, second]);
		expect(order).toEqual(["first:start", "first:end", "second:start", "second:end"]);
	});

	it("allows different files to proceed in parallel", async () => {
		const order: string[] = [];

		await Promise.all([
			withFileMutationQueue("/tmp/file-mutation-queue-a", async () => {
				order.push("a:start");
				await delay(30);
				order.push("a:end");
			}),
			withFileMutationQueue("/tmp/file-mutation-queue-b", async () => {
				order.push("b:start");
				await delay(30);
				order.push("b:end");
			}),
		]);

		expect(order.indexOf("a:start")).toBeLessThan(order.indexOf("a:end"));
		expect(order.indexOf("b:start")).toBeLessThan(order.indexOf("b:end"));
		expect(order.indexOf("b:start")).toBeLessThan(order.indexOf("a:end"));
	});

	it("uses the same queue for symlink aliases", async () => {
		const dir = await createTempDir();
		const targetPath = join(dir, "target.txt");
		const symlinkPath = join(dir, "alias.txt");
		await writeFile(targetPath, "hello\n", "utf8");
		await symlink(targetPath, symlinkPath);

		const order: string[] = [];
		await Promise.all([
			withFileMutationQueue(targetPath, async () => {
				order.push("target:start");
				await delay(30);
				order.push("target:end");
			}),
			withFileMutationQueue(symlinkPath, async () => {
				order.push("alias:start");
				order.push("alias:end");
			}),
		]);

		expect(order).toEqual(["target:start", "target:end", "alias:start", "alias:end"]);
	});
});

describe("built-in edit tool", () => {
	it("keeps the physical queue closed after the edit wrapper aborts", async () => {
		const dir = await createTempDir();
		const path = join(dir, "late.txt");
		await writeFile(path, "old");
		const [started, release, written] = [createDeferred(), createDeferred(), createDeferred()];
		const signal = new AbortController();
		const tool = createEditTool(dir, {
			operations: {
				access,
				readFile,
				writeFile: async (path, text) => {
					started.resolve();
					await release.promise;
					await writeFile(path, text);
					written.resolve();
				},
			},
		});
		const result = tool.execute("late", { path, edits: [{ oldText: "old", newText: "late" }] }, signal.signal);
		const aborted = expect(result).rejects.toThrow(/aborted/);
		let next: Promise<unknown> | undefined;
		let drained = false;
		const drain = drainFileMutationQueue(path).then(() => {
			drained = true;
		});
		try {
			await started.promise;
			signal.abort();
			await aborted;
			next = withFileMutationQueue(path, async () => readFile(path, "utf8"));
			void next.catch(() => undefined);
			expect(await readFile(path, "utf8")).toBe("old");
			expect(drained).toBe(false);
		} finally {
			release.resolve();
			await written.promise;
			await drain;
			expect(await next).toBe("late");
		}
	});

	it("preserves both parallel edits on the same file", async () => {
		const dir = await createTempDir();
		const filePath = join(dir, "parallel-edit.txt");
		await writeFile(filePath, "alpha\nbeta\ngamma\n", "utf8");

		const editTool = createEditTool(dir, {
			operations: {
				access,
				readFile: async (path) => {
					const buffer = await readFile(path);
					await delay(30);
					return buffer;
				},
				writeFile: async (path, content) => {
					await delay(30);
					await writeFile(path, content, "utf8");
				},
			},
		});

		await Promise.all([
			editTool.execute("call-1", { path: filePath, edits: [{ oldText: "alpha", newText: "ALPHA" }] }),
			editTool.execute("call-2", { path: filePath, edits: [{ oldText: "beta", newText: "BETA" }] }),
		]);

		const content = await readFile(filePath, "utf8");
		expect(content).toBe("ALPHA\nBETA\ngamma\n");
	});
});
