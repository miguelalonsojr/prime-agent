import type { ImageContent, TextContent } from "@earendil-works/pi-ai";
import { convertToLlm } from "../messages.js";
import type { ClmBlock, ClmSnapshot } from "../session-manager.js";

const END = "[[CTX_END]]";
const TURN = /^\[\[CTX_TURN id=(\S+) role=(\w+)\]\]$/;
const PROTECTED = /^\[\[CTX_PROTECTED id=(\S+)\]\]$/;
const IMAGE = /^\[\[CTX_IMAGE id=(\S+)\]\]$/;

type Content = (TextContent | ImageContent)[];
interface DocumentBlock {
	base: ClmBlock;
	body: string;
	role: string;
	protected: boolean;
}

function escapeText(text: string): string {
	return text
		.split("\n")
		.map((line) => line.replace(/^(?=\\|\[\[CTX_)/, "\\"))
		.join("\n");
}

function renderBody(block: ClmBlock): string {
	const message = convertToLlm([block.message])[0];
	if (!message) return escapeText(JSON.stringify(block.message));
	if (typeof message.content === "string") return escapeText(message.content);
	const content = message.content.map((item, index) => {
		switch (item.type) {
			case "text":
				return escapeText(item.text);
			case "image":
				return `[[CTX_IMAGE id=${encodeURIComponent(block.id)}:${index}]]`;
			case "thinking":
				return escapeText(`Thinking: ${item.thinking}`);
			case "toolCall":
				return escapeText(`Tool call ${item.id} ${item.name}: ${JSON.stringify(item.arguments)}`);
			default:
				throw new Error("Unsupported CLM content type");
		}
	});
	if (message.role === "toolResult")
		content.unshift(escapeText(`Tool result ${message.toolCallId} ${message.toolName}`));
	return content.join("\n");
}

/** Protected references are opaque; the caller must also verify their retention after parsing. */
export function renderContextDocument(snapshot: ClmSnapshot, protectedIds: ReadonlySet<string>): string {
	return snapshot.blocks
		.map((block) => {
			const id = encodeURIComponent(block.id);
			if (protectedIds.has(block.id)) return `[[CTX_PROTECTED id=${id}]]\n`;
			return `[[CTX_TURN id=${id} role=${block.message.role}]]\n${renderBody(block)}\n${END}\n`;
		})
		.join("");
}

function baseRegistry(base: ClmSnapshot): Map<string, ClmBlock> {
	const blocks = new Map<string, ClmBlock>();
	for (const block of base.blocks) {
		if (!block.id || blocks.has(encodeURIComponent(block.id))) throw new Error("Invalid or duplicate CLM block ID");
		blocks.set(encodeURIComponent(block.id), block);
	}
	return blocks;
}

function readBlocks(text: string, registry: Map<string, ClmBlock>): DocumentBlock[] {
	const blocks: DocumentBlock[] = [];
	const seen = new Set<string>();
	const lines = text.split("\n");
	for (let index = 0; index < lines.length; index++) {
		const line = lines[index];
		if (line === "") continue;
		const protectedMatch = PROTECTED.exec(line);
		const header = protectedMatch ?? TURN.exec(line);
		if (!header) throw new Error("Malformed CLM block header or protected reference");
		const base = registry.get(header[1]);
		if (!base || seen.has(base.id)) throw new Error("Unknown or duplicate CLM block reference");
		seen.add(base.id);
		if (protectedMatch) {
			blocks.push({ base, body: renderBody(base), role: base.message.role, protected: true });
			continue;
		}
		const body: string[] = [];
		while (++index < lines.length && lines[index] !== END) body.push(lines[index]);
		if (index === lines.length) throw new Error("Unterminated CLM block");
		blocks.push({ base, body: body.join("\n"), role: header[2], protected: false });
	}
	return blocks;
}

function imageRegistry(block: ClmBlock): Map<string, ImageContent> {
	const images = new Map<string, ImageContent>();
	const message = block.message;
	if (!("content" in message) || typeof message.content === "string") return images;
	for (const [index, content] of message.content.entries()) {
		if (content.type === "image") images.set(`${encodeURIComponent(block.id)}:${index}`, content);
	}
	return images;
}

function parseBody(body: string, images: Map<string, ImageContent>): Content {
	const content: Content = [];
	let text: string[] = [];
	const flushText = () => {
		if (text.length > 0) content.push({ type: "text", text: text.join("\n") });
		text = [];
	};
	for (const line of body.split("\n")) {
		if (line.startsWith("[[CTX_IMAGE")) {
			const reference = IMAGE.exec(line);
			const image = reference && images.get(reference[1]);
			if (!image) throw new Error("Unknown or malformed CLM image reference");
			flushText();
			content.push(structuredClone(image));
		} else {
			if (line.startsWith("[[CTX_PROTECTED")) throw new Error("Malformed CLM protected reference");
			text.push(line.replace(/^\\(?=\\|\[\[CTX_)/, ""));
		}
	}
	flushText();
	return content;
}

function unchanged(block: DocumentBlock): boolean {
	return block.role === block.base.message.role && block.body === renderBody(block.base);
}

/** Pair by occurrence and adjacency, never by a session-wide tool-call ID lookup. */
function toolGroups(base: ClmSnapshot): ClmBlock[][] {
	const groups: ClmBlock[][] = [];
	for (let index = 0; index < base.blocks.length; index++) {
		const block = base.blocks[index];
		if (block.message.role === "toolResult") throw new Error("Orphan CLM tool result in base snapshot");
		if (block.message.role !== "assistant") continue;
		const calls = block.message.content.filter((item) => item.type === "toolCall");
		if (calls.length === 0) continue;
		const pending = new Map(calls.map((call) => [call.id, call.name]));
		if (pending.size !== calls.length) throw new Error("Duplicate CLM tool-call ID within a group");
		const group = [block];
		for (let offset = 0; offset < calls.length; offset++) {
			const result = base.blocks[++index];
			if (
				!result ||
				result.message.role !== "toolResult" ||
				pending.get(result.message.toolCallId) !== result.message.toolName
			) {
				throw new Error("Incomplete or mismatched CLM tool group in base snapshot");
			}
			pending.delete(result.message.toolCallId);
			group.push(result);
		}
		groups.push(group);
	}
	return groups;
}

function changedToolBlocks(base: ClmSnapshot, blocks: DocumentBlock[]): Set<string> {
	const positions = new Map(blocks.map((block, index) => [block.base.id, index]));
	const changed = new Set<string>();
	for (const group of toolGroups(base)) {
		const start = positions.get(group[0].id);
		const intact =
			start !== undefined &&
			group.every((block, offset) => {
				const candidate = blocks[start + offset];
				return candidate?.base.id === block.id && unchanged(candidate);
			});
		if (!intact) for (const block of group) changed.add(block.id);
	}
	return changed;
}

export function parseContextDocument(text: string, base: ClmSnapshot): ClmSnapshot {
	const blocks = readBlocks(text, baseRegistry(base));
	const changed = changedToolBlocks(base, blocks);
	return {
		blocks: blocks.map((block): ClmBlock => {
			const content = parseBody(block.body, imageRegistry(block.base));
			if (unchanged(block) && !changed.has(block.base.id)) return structuredClone(block.base);
			if (block.protected) throw new Error("Edited CLM tool group contains a protected reference");
			if (block.role !== block.base.message.role)
				content.unshift({
					type: "text",
					text: `[[CTX_TURN id=${encodeURIComponent(block.base.id)} role=${block.role}]]`,
				});
			if (block.base.message.role !== "user")
				content.unshift({
					type: "text",
					text: `[Historical ${block.base.message.role}]`,
				});
			return { id: block.base.id, message: { role: "user", content, timestamp: block.base.message.timestamp } };
		}),
	};
}
