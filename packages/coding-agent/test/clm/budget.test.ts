import type { AgentMessage } from "@earendil-works/pi-agent-core";
import { getModel, type Tool, type UserMessage } from "@earendil-works/pi-ai";
import { Type } from "typebox";
import { describe, expect, it } from "vitest";
import { estimateClmRequestTokens } from "../../src/core/clm/budget.js";
import { emptyUsage } from "../../src/core/usage.js";

const model = { ...getModel("openai", "gpt-5")!, contextWindow: 1000, maxTokens: 300 };
const user = (content: UserMessage["content"]): UserMessage => ({ role: "user", content, timestamp: 0 });
const estimate = (messages: AgentMessage[], systemPrompt = "", tools: Tool[] = [], selectedModel = model) =>
	estimateClmRequestTokens({ messages, systemPrompt, tools, model: selectedModel });

const assistant = (
	content: Extract<AgentMessage, { role: "assistant" }>["content"] = [],
): Extract<AgentMessage, { role: "assistant" }> => ({
	role: "assistant",
	content,
	api: model.api,
	provider: model.provider,
	model: model.id,
	usage: emptyUsage(),
	stopReason: "stop",
	timestamp: 0,
});

describe("CLM request budget", () => {
	it("estimates fixed instructions, tools, and images before a request", () => {
		const prompt = "fixed instructions".repeat(30);
		const schema = Type.Object({ query: Type.String() });
		const tools = [{ name: "search", description: "find records", parameters: schema }];
		const base = estimate([user("hello")]).estimatedInputTokens;
		const fixed = estimate([user("hello")], prompt).estimatedInputTokens;
		const withTools = estimate([user("hello")], prompt, tools).estimatedInputTokens;
		const image = user([{ type: "image", data: "a".repeat(8000), mimeType: "image/png" }]);
		const withImage = estimate([image], prompt, tools).estimatedInputTokens;
		expect(fixed).toBeGreaterThan(base);
		expect(withTools).toBeGreaterThan(fixed);
		expect(withImage).toBeGreaterThan(withTools);
		expect(
			estimate([user("hello")], prompt, [{ ...tools[0], parameters: Type.Object({}) }]).estimatedInputTokens,
		).toBeLessThan(withTools);
		expect(
			estimate([user([{ type: "image", data: "a", mimeType: "image/png" }])], prompt, tools).estimatedInputTokens,
		).toBeLessThan(withImage);
	});

	it("counts current content instead of substituting historical provider usage", () => {
		const priorAssistant = {
			...assistant([{ type: "text", text: "brief reply" }]),
			usage: { ...emptyUsage(), input: 900000 },
		};
		const prior = estimate([user("old"), priorAssistant]).estimatedInputTokens;
		expect(prior).toBeLessThan(100);
		expect(
			estimate([user("old"), priorAssistant, user("fresh context".repeat(80))]).estimatedInputTokens,
		).toBeGreaterThan(prior);
	});

	it("counts the provider-visible digest in a compaction summary", () => {
		const summary: AgentMessage = { role: "compactionSummary", summary: "summary", tokensBefore: 20, timestamp: 0 };
		const without = estimate([summary]).estimatedInputTokens;
		const withDigest = estimate([{ ...summary, harnessDigest: "critical memory ".repeat(300) }]).estimatedInputTokens;
		expect(withDigest).toBeGreaterThan(without);
	});

	it("counts semantic content across message roles", () => {
		const toolResult: AgentMessage = {
			role: "toolResult",
			toolCallId: "a",
			toolName: "read",
			content: [],
			isError: false,
			timestamp: 0,
		};
		const growth = (short: AgentMessage, long: AgentMessage) =>
			expect(estimate([long]).estimatedInputTokens).toBeGreaterThan(estimate([short]).estimatedInputTokens);
		growth(toolResult, { ...toolResult, content: [{ type: "text", text: "result".repeat(100) }] });
		growth(toolResult, {
			...toolResult,
			content: [{ type: "image", data: "a".repeat(8000), mimeType: "image/png" }],
		});
		growth(assistant(), assistant([{ type: "thinking", thinking: "thought".repeat(100) }]));
		growth(
			assistant(),
			assistant([{ type: "toolCall", id: "a", name: "read", arguments: { path: "filename".repeat(100) } }]),
		);
		const custom: AgentMessage = { role: "custom", customType: "note", content: "", display: true, timestamp: 0 };
		growth(custom, { ...custom, content: "note".repeat(100) });
		const branch: AgentMessage = { role: "branchSummary", summary: "", fromId: "a", timestamp: 0 };
		growth(branch, { ...branch, summary: "branch".repeat(100) });
		const summary: AgentMessage = { role: "compactionSummary", summary: "", tokensBefore: 0, timestamp: 0 };
		growth(summary, { ...summary, summary: "compact".repeat(100) });
	});

	it("reserves output within a known window", () => {
		expect(estimate([])).toMatchObject({ outputReserveTokens: 250, maxInputTokens: 750 });
		expect(estimate([], "", [], { ...model, maxTokens: 120 })).toMatchObject({
			outputReserveTokens: 120,
			maxInputTokens: 880,
		});
	});

	it("estimates input without a hard gate for an unknown window", () => {
		const result = estimate([user("content".repeat(100))], "fixed", [], { ...model, contextWindow: 0 });
		expect(result.estimatedInputTokens).toBeGreaterThan(0);
		expect(result.outputReserveTokens).toBeNull();
		expect(result.maxInputTokens).toBeNull();
	});
});
