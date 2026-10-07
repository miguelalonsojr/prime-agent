import type { AgentMessage } from "@earendil-works/pi-agent-core";
import type { Api, Model, Tool } from "@earendil-works/pi-ai";

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

function estimateContent(
	content: string | Array<{ type: string; text?: string; data?: string; mimeType?: string }>,
): number {
	if (typeof content === "string") return Math.ceil(content.length / 4);
	return content.reduce((total, block) => {
		if (block.type === "image") return total + estimateImage(block.data ?? "");
		return total + Math.ceil((block.text?.length ?? 0) / 4);
	}, 0);
}

function estimateMessage(message: AgentMessage): number {
	switch (message.role) {
		case "user":
		case "toolResult":
		case "custom":
			return estimateContent(message.content);
		case "assistant":
			return message.content.reduce((total, block) => total + estimateAssistantBlock(block), 0);
		case "bashExecution":
			return Math.ceil((message.command.length + message.output.length) / 4);
		case "branchSummary":
		case "compactionSummary":
			return Math.ceil(message.summary.length / 4);
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
			return Math.ceil((block.name.length + JSON.stringify(block.arguments).length) / 4);
	}
}
