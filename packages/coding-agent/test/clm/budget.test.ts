import type { AgentMessage } from "@earendil-works/pi-agent-core";
import type { Api, Model, Tool, UserMessage } from "@earendil-works/pi-ai";
import { Type } from "typebox";
import { describe, expect, it } from "vitest";
import { estimateClmRequestTokens } from "../../src/core/clm/budget.js";

const model: Model<Api> = {
	id: "budget",
	name: "Budget",
	api: "openai-responses",
	provider: "openai",
	baseUrl: "https://example.invalid",
	reasoning: false,
	input: ["text", "image"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 1000,
	maxTokens: 300,
};
const user = (content: UserMessage["content"]): UserMessage => ({ role: "user", content, timestamp: 0 });
const estimate = (messages: AgentMessage[], systemPrompt = "", tools: Tool[] = [], selectedModel = model) =>
	estimateClmRequestTokens({ messages, systemPrompt, tools, model: selectedModel });

describe("CLM request budget", () => {
	it("estimates fixed instructions, tools, and images before a request", () => {
		const prompt = "fixed instructions".repeat(30);
		const tools = [
			{ name: "search", description: "find records", parameters: Type.Object({ query: Type.String() }) },
		];
		const base = estimate([user("hello")]).estimatedInputTokens;
		const fixed = estimate([user("hello")], prompt).estimatedInputTokens;
		const withTools = estimate([user("hello")], prompt, tools).estimatedInputTokens;
		const withImage = estimate(
			[user([{ type: "image", data: "a".repeat(8000), mimeType: "image/png" }])],
			prompt,
			tools,
		).estimatedInputTokens;
		expect(fixed).toBeGreaterThan(base);
		expect(withTools).toBeGreaterThan(fixed);
		expect(withImage).toBeGreaterThan(withTools);
	});

	it("counts current content instead of substituting historical provider usage", () => {
		const assistant: AgentMessage = {
			role: "assistant",
			content: [{ type: "text", text: "brief reply" }],
			api: model.api,
			provider: model.provider,
			model: model.id,
			usage: {
				input: 900000,
				output: 100,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 900100,
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
			},
			stopReason: "stop",
			timestamp: 0,
		};
		const prior = estimate([user("old"), assistant]).estimatedInputTokens;
		expect(prior).toBeLessThan(100);
		expect(estimate([user("old"), assistant, user("fresh context".repeat(80))]).estimatedInputTokens).toBeGreaterThan(
			prior,
		);
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
