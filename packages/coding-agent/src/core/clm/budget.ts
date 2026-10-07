import type { AgentMessage } from "@earendil-works/pi-agent-core";
import type { Api, ImageContent, Model, TextContent, Tool } from "@earendil-works/pi-ai";
import {
	BRANCH_SUMMARY_PREFIX,
	BRANCH_SUMMARY_SUFFIX,
	COMPACTION_SUMMARY_PREFIX,
	COMPACTION_SUMMARY_SUFFIX,
	HARNESS_DIGEST_PREFIX,
	HARNESS_DIGEST_SUFFIX,
} from "../messages.js";

export interface ClmRequestBudget {
	estimatedInputTokens: number;
	outputReserveTokens: number | null;
	maxInputTokens: number | null;
}

/** Estimate semantic request content, independently of billed usage from older requests. */
export function estimateClmRequestTokens({
	messages,
	systemPrompt,
	tools,
	model,
}: {
	messages: AgentMessage[];
	systemPrompt: string;
	tools: Tool[];
	model: Model<Api>;
}): ClmRequestBudget {
	const chars = systemPrompt.length + estimateToolDefinitions(tools);
	const estimatedInputTokens =
		Math.ceil(chars / 4) + messages.reduce((total, message) => total + estimateMessage(message), 0);
	const outputReserveTokens =
		model.contextWindow > 0 ? Math.min(model.maxTokens, Math.floor(model.contextWindow / 4)) : null;
	return {
		estimatedInputTokens,
		outputReserveTokens,
		maxInputTokens: outputReserveTokens === null ? null : model.contextWindow - outputReserveTokens,
	};
}

function estimateToolDefinitions(tools: Tool[]): number {
	return tools.reduce(
		(total, tool) => total + tool.name.length + tool.description.length + JSON.stringify(tool.parameters).length,
		0,
	);
}

function estimateImage(data: string): number {
	// Image tokens vary by provider and resolution; count a floor plus payload growth.
	return Math.max(1200, Math.ceil(data.length / 4));
}

function estimateContent(content: string | (TextContent | ImageContent)[]): number {
	if (typeof content === "string") return Math.ceil(content.length / 4);
	return content.reduce((total, block) => total + estimateContentBlock(block), 0);
}

function estimateContentBlock(block: TextContent | ImageContent): number {
	switch (block.type) {
		case "text":
			return Math.ceil(block.text.length / 4);
		case "image":
			return estimateImage(block.data);
	}
}

function estimateMessage(message: AgentMessage): number {
	switch (message.role) {
		case "user":
		case "custom":
			return estimateContent(message.content);
		case "toolResult":
			return Math.ceil((message.toolCallId.length + message.toolName.length) / 4) + estimateContent(message.content);
		case "assistant":
			return message.content.reduce((total, block) => total + estimateAssistantBlock(block), 0);
		case "bashExecution":
			return Math.ceil((message.command.length + message.output.length) / 4);
		case "branchSummary":
			return estimateBranchSummary(message.summary);
		case "compactionSummary":
			return estimateCompactionSummary(message.summary, message.harnessDigest);
	}
	return 0;
}

function estimateAssistantBlock(block: Extract<AgentMessage, { role: "assistant" }>["content"][number]): number {
	switch (block.type) {
		case "text":
			return Math.ceil(block.text.length / 4);
		case "thinking":
			return Math.ceil(block.thinking.length / 4);
		case "toolCall":
			return Math.ceil((block.id.length + block.name.length + JSON.stringify(block.arguments).length) / 4);
	}
}

function estimateBranchSummary(summary: string): number {
	return Math.ceil((BRANCH_SUMMARY_PREFIX.length + summary.length + BRANCH_SUMMARY_SUFFIX.length) / 4);
}

function estimateCompactionSummary(summary: string, harnessDigest?: string): number {
	const digestLength = harnessDigest
		? HARNESS_DIGEST_PREFIX.length + harnessDigest.length + HARNESS_DIGEST_SUFFIX.length + 2
		: 0;
	return Math.ceil(
		(digestLength + COMPACTION_SUMMARY_PREFIX.length + summary.length + COMPACTION_SUMMARY_SUFFIX.length) / 4,
	);
}
