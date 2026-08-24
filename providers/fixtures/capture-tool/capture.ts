import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const PINNED_COMMIT = "c49906ec77788625aacbdc53ebca6fbe65bd20f5";
const FIXTURE_TIMESTAMP = 1_700_000_000_000;
const FIXTURE_SESSION_ID = "session-m4-00000000";
const FIXTURE_API_KEY = "fixture-api-key-never-forwarded";
const FIXTURE_CODEX_API_KEY =
	"eyJhbGciOiJub25lIn0.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjb3VudC1maXh0dXJlIn19.signature";
const TOOL_CALL_1 = "call_fixture_0001";
const TOOL_CALL_2 = "call_fixture_0002";
const PNG_1X1 =
	"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

const tool = {
	name: "read_file",
	description: "Read one UTF-8 file.",
	parameters: {
		type: "object",
		properties: { path: { type: "string", description: "Workspace-relative path." } },
		required: ["path"],
		additionalProperties: false,
	},
};

const strictTool = {
	...tool,
	constrainedSampling: { type: "json_schema", strict: "require" },
};

type Family =
	| "openai-completions"
	| "anthropic-messages"
	| "openai-responses"
	| "openai-codex-responses";
type ResponseKind = "text" | "tool" | "multiple-tools" | "signed-reasoning" | "redacted-reasoning";
type CaptureMode = "hermetic" | "credential-backed";

interface FixtureCase {
	name: string;
	description: string;
	context: Record<string, unknown>;
	options?: Record<string, unknown>;
	modelPatch?: Record<string, unknown>;
	responseKind?: ResponseKind;
	simple?: boolean;
}

interface CapturedRequest {
	method: string;
	path: string;
	headers: Record<string, string>;
	omittedRuntimeHeaders: string[];
	body: Uint8Array;
}

interface CaptureServerState {
	scriptedResponses: Uint8Array[];
	requests: CapturedRequest[];
	responseBodies: Uint8Array[];
	upstream?: ProxyUpstream;
}

interface ProxyUpstream {
	baseUrl: string;
	localBasePath: string;
}

interface CredentialBackedTarget {
	apiKey?: string;
	optionHeaders?: Record<string, string>;
	secretValues: string[];
	credentialSource: string;
	upstream: ProxyUpstream;
	modelPatch: Record<string, unknown>;
}

interface CredentialCaptureResult {
	family: Family;
	case: string;
	status: "captured" | "not-captured";
	reason?: string;
}

function isOpenAiFamily(family: Family): boolean {
	return family !== "anthropic-messages";
}

function isResponsesFamily(family: Family): boolean {
	return family === "openai-responses" || family === "openai-codex-responses";
}

function user(content: unknown): Record<string, unknown> {
	return { role: "user", content, timestamp: FIXTURE_TIMESTAMP };
}

function assistant(
	provider: string,
	api: string,
	model: string,
	content: unknown[],
	stopReason: string,
	extra: Record<string, unknown> = {},
): Record<string, unknown> {
	return {
		role: "assistant",
		content,
		api,
		provider,
		model,
		usage: {
			input: 12,
			output: 8,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: 20,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		},
		stopReason,
		timestamp: FIXTURE_TIMESTAMP,
		...extra,
	};
}

function toolResult(
	id: string,
	content: unknown[] = [{ type: "text", text: "fixture file contents" }],
): Record<string, unknown> {
	return {
		role: "toolResult",
		toolCallId: id,
		toolName: "read_file",
		content,
		isError: false,
		timestamp: FIXTURE_TIMESTAMP,
	};
}

function baseContext(): Record<string, unknown> {
	return { messages: [user("Return a concise fixture response.")] };
}

function toolHistoryContext(api: Family, provider: string, model: string): Record<string, unknown> {
	return {
		messages: [
			user("Read Cargo.toml."),
			assistant(
				provider,
				api,
				model,
				[
					{
						type: "toolCall",
						id: TOOL_CALL_1,
						name: "read_file",
						arguments: { path: "Cargo.toml" },
					},
				],
				"toolUse",
			),
			toolResult(TOOL_CALL_1),
			user("Summarize the result."),
		],
		tools: [tool],
	};
}

function commonCases(api: Family, provider: string, model: string): FixtureCase[] {
	const orphan = assistant(
		provider,
		api,
		model,
		[
			{
				type: "toolCall",
				id: TOOL_CALL_1,
				name: "read_file",
				arguments: { path: "Cargo.toml" },
			},
		],
		"toolUse",
	);
	const failed = assistant(provider, api, model, [{ type: "text", text: "partial secret-free text" }], "error", {
		errorMessage: "fixture transport failure",
	});

	return [
		{
			name: "text-only",
			description: "One user text message and a text response.",
			context: baseContext(),
		},
		{
			name: "system-developer-prompt",
			description: "System prompt lowering, including OpenAI developer-role selection.",
			context: { ...baseContext(), systemPrompt: "Fixture system instruction." },
			modelPatch: api === "openai-completions" ? { compat: { supportsDeveloperRole: true } } : {},
		},
		{
			name: "images",
			description: "User text followed by one deterministic PNG data block.",
			context: {
				messages: [
					user([
						{ type: "text", text: "Describe this fixture image." },
						{ type: "image", mimeType: "image/png", data: PNG_1X1 },
					]),
				],
			},
		},
		{
			name: "thinking-disabled",
			description: "Reasoning-capable model with simple reasoning omitted.",
			context: baseContext(),
			simple: true,
		},
		...(["minimal", "low", "medium", "high", "xhigh", "max"] as const).map(
			(level): FixtureCase => ({
				name: `reasoning-${level}`,
				description: `Simple reasoning level ${level}.`,
				context: baseContext(),
				options: { reasoning: level, maxTokens: 4096 },
				modelPatch: api === "anthropic-messages" ? { compat: { forceAdaptiveThinking: true } } : {},
				simple: true,
			}),
		),
		{
			name: "signed-thinking-replay",
			description: "Turn one returns signed reasoning and a tool call for exact turn-two replay.",
			context: { messages: [user("Reason, then read Cargo.toml.")], tools: [tool] },
			options: { reasoning: "low", maxTokens: 1024 },
			responseKind: "signed-reasoning",
			simple: true,
		},
		{
			name: "redacted-encrypted-reasoning-replay",
			description: "Turn one returns redacted/encrypted reasoning and a tool call.",
			context: { messages: [user("Privately reason, then read Cargo.toml.")], tools: [tool] },
			options: { reasoning: "low", maxTokens: 1024 },
			responseKind: "redacted-reasoning",
			simple: true,
		},
		{
			name: "one-tool-call",
			description: "One streamed function/tool call followed by its result on turn two.",
			context: { messages: [user("Read Cargo.toml.")], tools: [tool] },
			responseKind: "tool",
		},
		{
			name: "multiple-tool-calls",
			description: "Two streamed tool calls retained in assistant source order.",
			context: { messages: [user("Read Cargo.toml and README.md.")], tools: [tool] },
			responseKind: "multiple-tools",
		},
		{
			name: "tool-results",
			description: "Existing assistant tool call and text tool result.",
			context: toolHistoryContext(api, provider, model),
		},
		{
			name: "tool-result-images",
			description: "Tool result containing text and a deterministic PNG.",
			context: {
				messages: [
					user("Read an image."),
					assistant(
						provider,
						api,
						model,
						[
							{
								type: "toolCall",
								id: TOOL_CALL_1,
								name: "read_file",
								arguments: { path: "fixture.png" },
							},
						],
						"toolUse",
					),
					toolResult(TOOL_CALL_1, [
						{ type: "text", text: "fixture image" },
						{ type: "image", mimeType: "image/png", data: PNG_1X1 },
					]),
					user("Describe it."),
				],
				tools: [tool],
			},
		},
		{
			name: "orphan-result-repair",
			description: "Missing tool result is synthesized before the following user message.",
			context: { messages: [user("Read Cargo.toml."), orphan, user("Continue without the result.")], tools: [tool] },
		},
		{
			name: "cache-disabled",
			description: "Prompt caching explicitly disabled.",
			context: { ...baseContext(), systemPrompt: "Cache fixture." },
			options: { cacheRetention: "none", sessionId: FIXTURE_SESSION_ID },
		},
		{
			name: "cache-short",
			description: "Default short prompt-cache markers.",
			context: { ...baseContext(), systemPrompt: "Cache fixture." },
			options: { cacheRetention: "short", sessionId: FIXTURE_SESSION_ID },
			modelPatch: api === "openai-completions"
				? { compat: { cacheControlFormat: "anthropic", supportsLongCacheRetention: true } }
				: {},
		},
		{
			name: "cache-long",
			description: "Long prompt-cache markers and one-hour TTL where supported.",
			context: { ...baseContext(), systemPrompt: "Cache fixture." },
			options: { cacheRetention: "long", sessionId: FIXTURE_SESSION_ID },
			modelPatch: api === "openai-completions"
				? { compat: { cacheControlFormat: "anthropic", supportsLongCacheRetention: true } }
				: { compat: { supportsLongCacheRetention: true } },
		},
		{
			name: "sampling-defaults-and-overrides",
			description: "Catalog sampling defaults overlaid by request sampling values.",
			context: baseContext(),
			options: { temperature: 0, samplingParams: { temperature: 0.75, top_p: 0.6, seed: 7 } },
			modelPatch: { samplingParams: { temperature: 1, top_k: 40 } },
			simple: true,
		},
		{
			name: "max-output-clamp",
			description: "Simple max-output planning under the 4096-token context reserve.",
			context: baseContext(),
			options: { maxTokens: 9000 },
			modelPatch: { contextWindow: 4200, maxTokens: 2048 },
			simple: true,
		},
		{
			name: "strict-tool-schema",
			description: "Required strict JSON-schema constrained sampling.",
			context: { messages: [user("Read Cargo.toml.")], tools: [strictTool] },
			modelPatch: isOpenAiFamily(api)
				? { compat: { supportsStrictMode: true } }
				: { compat: { supportsStrictTools: true } },
		},
		{
			name: "provider-model-headers",
			description: "Model and explicit request headers captured separately from body bytes.",
			context: baseContext(),
			options: { headers: { "x-fixture-request": "request-value" } },
			modelPatch: { headers: { "x-fixture-model": "model-value" } },
		},
		{
			name: "session-affinity",
			description: "Deterministically injected session affinity identifier.",
			context: baseContext(),
			options: { sessionId: FIXTURE_SESSION_ID },
			modelPatch: api === "openai-completions"
				? { compat: { sendSessionAffinityHeaders: true, sessionAffinityFormat: "openai" } }
				: api === "anthropic-messages"
					? { compat: { sendSessionAffinityHeaders: true } }
					: api === "openai-responses"
						? { compat: { sessionAffinityFormat: "openai" } }
						: {},
		},
		{
			name: "api-specific-compat-flags",
			description: "One family-specific compatibility branch made observable on wire.",
			context: toolHistoryContext(api, provider, model),
			modelPatch: api === "openai-completions"
				? { compat: { requiresAssistantAfterToolResult: true, requiresToolResultName: true } }
				: api === "anthropic-messages"
					? { compat: { allowEmptySignature: true, supportsEagerToolInputStreaming: false } }
					: { compat: { supportsStrictMode: false } },
		},
		{
			name: "cross-provider-handoff",
			description: "Foreign assistant thinking is projected to ordinary text.",
			context: {
				messages: [
					user("Think about the fixture."),
					assistant(
						"foreign-provider",
						"foreign-api",
						"foreign-model",
						[
							{ type: "thinking", thinking: "Foreign visible reasoning.", thinkingSignature: "foreign-signature" },
							{ type: "text", text: "Foreign answer." },
						],
						"stop",
					),
					user("Continue on the target model."),
				],
			},
		},
		{
			name: "failed-turn-omission",
			description: "Failed assistant remains canonical but is omitted from provider projection.",
			context: { messages: [user("First attempt."), failed, user("Retry cleanly.")] },
		},
	];
}

function mergeNested(base: Record<string, unknown>, patch: Record<string, unknown> = {}): Record<string, unknown> {
	const merged = { ...base, ...patch };
	if (base.compat || patch.compat) {
		merged.compat = { ...(base.compat as object | undefined), ...(patch.compat as object | undefined) };
	}
	if (base.headers || patch.headers) {
		merged.headers = { ...(base.headers as object | undefined), ...(patch.headers as object | undefined) };
	}
	return merged;
}

function modelFor(family: Family, baseUrl: string, patch: Record<string, unknown> = {}): Record<string, unknown> {
	const openAi = isOpenAiFamily(family);
	const responses = isResponsesFamily(family);
	const codex = family === "openai-codex-responses";
	const base: Record<string, unknown> = {
		id: codex
			? "fixture-codex-model"
			: openAi
				? "fixture-openai-model"
				: "fixture-anthropic-model",
		name: codex ? "Fixture Codex Model" : openAi ? "Fixture OpenAI Model" : "Fixture Anthropic Model",
		api: family,
		provider: codex ? "openai-codex" : openAi ? "fixture-openai" : "fixture-anthropic",
		baseUrl,
		reasoning: true,
		input: ["text", "image"],
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: 128_000,
		maxTokens: 8192,
		thinkingLevelMap: {
			off: openAi ? "none" : "off",
			minimal: "minimal",
			low: "low",
			medium: "medium",
			high: "high",
			xhigh: "xhigh",
			max: "max",
		},
		compat: responses
			? {
					supportsDeveloperRole: true,
					supportsReasoningEffort: true,
					supportsStrictMode: true,
					supportsLongCacheRetention: true,
					supportsOpenAIGrammarTools: false,
				}
			: openAi
			? {
					supportsStore: false,
					supportsDeveloperRole: false,
					supportsReasoningEffort: true,
					supportsUsageInStreaming: true,
					supportsFinishReason: true,
					maxTokensField: "max_tokens",
					supportsStrictMode: true,
					supportsLongCacheRetention: true,
				}
			: {
					supportsLongCacheRetention: true,
					supportsCacheControlOnTools: true,
					supportsTemperature: true,
					supportsStrictTools: true,
				},
	};
	return mergeNested(base, patch);
}

function readPiAuthStore(): Record<string, unknown> {
	const path = join(homedir(), ".pi", "agent", "auth.json");
	if (!existsSync(path)) return {};
	const parsed = JSON.parse(readFileSync(path, "utf8")) as unknown;
	return typeof parsed === "object" && parsed !== null && !Array.isArray(parsed)
		? parsed as Record<string, unknown>
		: {};
}

function storedCredential(store: Record<string, unknown>, provider: string): Record<string, unknown> | undefined {
	const value = store[provider];
	return typeof value === "object" && value !== null && !Array.isArray(value)
		? value as Record<string, unknown>
		: undefined;
}

function copilotBaseUrl(accessToken: string): string {
	const match = accessToken.match(/(?:^|;)proxy-ep=([^;]+)/);
	if (!match) return "https://api.githubcopilot.com";
	return `https://${match[1].replace(/^proxy\./, "api.")}`;
}

async function refreshCopilotCredential(store: Record<string, unknown>): Promise<{ access: string; baseUrl: string }> {
	const credential = storedCredential(store, "github-copilot");
	const refresh = credential?.refresh;
	if (typeof refresh !== "string" || refresh.length === 0) {
		throw new Error("Pi auth store has no github-copilot OAuth refresh credential");
	}
	const response = await fetch("https://api.github.com/copilot_internal/v2/token", {
		headers: {
			accept: "application/json",
			authorization: `Bearer ${refresh}`,
			"copilot-integration-id": "vscode-chat",
			"editor-plugin-version": "copilot-chat/0.35.0",
			"editor-version": "vscode/1.107.0",
			"user-agent": "GitHubCopilotChat/0.35.0",
		},
	});
	if (!response.ok) {
		throw new Error(`github-copilot OAuth refresh returned HTTP ${response.status}`);
	}
	const data = await response.json() as Record<string, unknown>;
	const access = data.token;
	if (typeof access !== "string" || access.length === 0) {
		throw new Error("github-copilot OAuth refresh returned no access token");
	}
	return { access, baseUrl: copilotBaseUrl(access) };
}

async function credentialTargetFor(
	family: Family,
	store: Record<string, unknown>,
): Promise<CredentialBackedTarget> {
	if (isResponsesFamily(family)) {
		throw new Error(`${family} credential-backed capture is not configured; use hermetic capture`);
	}
	if (family === "openai-completions") {
		const apiKey = process.env.OPENROUTER_API_KEY;
		if (!apiKey) throw new Error("OPENROUTER_API_KEY is unavailable");
		return {
			apiKey,
			secretValues: [apiKey],
			credentialSource: "OPENROUTER_API_KEY",
			upstream: {
				baseUrl: "https://openrouter.ai/api/v1",
				localBasePath: "/v1",
			},
			modelPatch: {
				id: process.env.PI_FIXTURE_OPENROUTER_MODEL ?? "openai/gpt-oss-20b",
				name: "Credential-backed OpenRouter fixture model",
				provider: "openrouter",
				input: ["text"],
				compat: {
					supportsStore: false,
					supportsDeveloperRole: true,
					supportsReasoningEffort: true,
					supportsUsageInStreaming: true,
					supportsFinishReason: true,
					maxTokensField: "max_tokens",
					thinkingFormat: "openrouter",
					supportsStrictMode: true,
				},
			},
		};
	}

	if ((process.env.PI_FIXTURE_ANTHROPIC_CREDENTIAL ?? "openrouter") !== "github-copilot") {
		const apiKey = process.env.OPENROUTER_API_KEY;
		if (!apiKey) throw new Error("OPENROUTER_API_KEY is unavailable for the Anthropic Messages endpoint");
		return {
			optionHeaders: { authorization: `Bearer ${apiKey}` },
			secretValues: [apiKey],
			credentialSource: "OPENROUTER_API_KEY (Anthropic Messages compatibility endpoint)",
			upstream: {
				baseUrl: "https://openrouter.ai/api",
				localBasePath: "",
			},
			modelPatch: {
				id: process.env.PI_FIXTURE_OPENROUTER_ANTHROPIC_MODEL ?? "anthropic/claude-haiku-4.5",
				name: "Credential-backed OpenRouter Anthropic fixture model",
				provider: "openrouter",
				input: ["text", "image"],
				maxTokens: 4096,
				compat: {
					supportsEagerToolInputStreaming: false,
					supportsLongCacheRetention: false,
					supportsCacheControlOnTools: false,
					supportsTemperature: true,
					supportsStrictTools: false,
					supportsToolReferences: false,
				},
			},
		};
	}

	const copilot = await refreshCopilotCredential(store);
	return {
		apiKey: copilot.access,
		secretValues: [copilot.access],
		credentialSource: "~/.pi/agent/auth.json:github-copilot OAuth (refreshed in memory)",
		upstream: {
			baseUrl: copilot.baseUrl,
			localBasePath: "",
		},
		modelPatch: {
			id: process.env.PI_FIXTURE_COPILOT_ANTHROPIC_MODEL ?? "claude-haiku-4.5",
			name: "Credential-backed GitHub Copilot Anthropic fixture model",
			provider: "github-copilot",
			input: ["text", "image"],
			headers: {
				"copilot-integration-id": "vscode-chat",
				"editor-plugin-version": "copilot-chat/0.35.0",
				"editor-version": "vscode/1.107.0",
				"user-agent": "GitHubCopilotChat/0.35.0",
			},
			compat: {
				supportsEagerToolInputStreaming: false,
				supportsLongCacheRetention: false,
				supportsCacheControlOnTools: false,
				supportsTemperature: true,
				supportsStrictTools: false,
				supportsToolReferences: false,
			},
		},
	};
}

function withCredentialToolChoice(family: Family, fixture: FixtureCase): FixtureCase {
	const needsToolChoice = fixture.responseKind === "tool" ||
		(family === "openai-completions" && fixture.responseKind === "signed-reasoning");
	const toolChoice = needsToolChoice
		? family === "openai-completions"
			? { type: "function", function: { name: "read_file" } }
			: { type: "tool", name: "read_file" }
		: undefined;
	return {
		...fixture,
		...(family === "openai-completions" ? { simple: true } : {}),
		options: {
			...fixture.options,
			...(family === "openai-completions"
				? { reasoning: fixture.options?.reasoning ?? "low", maxTokens: fixture.options?.maxTokens ?? 1024 }
				: {}),
			...(toolChoice ? { toolChoice } : {}),
		},
	};
}

function openAiFrames(kind: ResponseKind, model: string, turn: number): string {
	const id = `chatcmpl-fixture-${turn}`;
	const chunk = (delta: Record<string, unknown>, finishReason: string | null = null) => ({
		id,
		object: "chat.completion.chunk",
		created: 1_700_000_000,
		model,
		choices: [{ index: 0, delta, finish_reason: finishReason }],
	});
	const chunks: unknown[] = [];
	if (kind === "text") {
		chunks.push(chunk({ role: "assistant", content: `fixture response turn ${turn}` }));
		chunks.push(chunk({}, "stop"));
	} else {
		if (kind === "signed-reasoning" || kind === "redacted-reasoning") {
			const detail =
				kind === "signed-reasoning"
					? {
						type: "reasoning.text",
						id: "reasoning-fixture-1",
						format: "fixture-v1",
						index: 0,
						text: "Inspect the requested fixture.",
						signature: "signed-fixture-reasoning",
					}
					: {
						type: "reasoning.encrypted",
						id: "reasoning-fixture-1",
						format: "fixture-v1",
						index: 0,
						data: "encrypted-fixture-reasoning",
					};
			chunks.push(
				chunk({
					reasoning: kind === "signed-reasoning" ? "Inspect the requested fixture." : undefined,
					reasoning_details: [detail],
				}),
			);
		}
		const calls = [
			{
				index: 0,
				id: TOOL_CALL_1,
				type: "function",
				function: { name: "read_file", arguments: '{"path":"Cargo.toml"}' },
			},
		];
		if (kind === "multiple-tools") {
			calls.push({
				index: 1,
				id: TOOL_CALL_2,
				type: "function",
				function: { name: "read_file", arguments: '{"path":"README.md"}' },
			});
		}
		chunks.push(chunk({ tool_calls: calls }));
		chunks.push(chunk({}, "tool_calls"));
	}
	chunks.push({
		id,
		object: "chat.completion.chunk",
		created: 1_700_000_000,
		model,
		choices: [],
		usage: {
			prompt_tokens: 12,
			completion_tokens: 8,
			total_tokens: 20,
			completion_tokens_details: { reasoning_tokens: kind.includes("reasoning") ? 3 : 0 },
		},
	});
	return `${chunks.map((value) => `data: ${JSON.stringify(value)}\n\n`).join("")}data: [DONE]\n\n`;
}

function anthropicFrames(kind: ResponseKind, model: string, turn: number): string {
	const events: Array<{ event: string; data: unknown }> = [];
	const add = (event: string, data: unknown) => events.push({ event, data });
	add("message_start", {
		type: "message_start",
		message: {
			id: `msg_fixture_${turn}`,
			type: "message",
			role: "assistant",
			model,
			content: [],
			stop_reason: null,
			stop_sequence: null,
			usage: {
				input_tokens: 12,
				output_tokens: 0,
				cache_read_input_tokens: 0,
				cache_creation_input_tokens: 0,
			},
		},
	});

	if (kind === "text") {
		add("content_block_start", {
			type: "content_block_start",
			index: 0,
			content_block: { type: "text", text: "" },
		});
		add("content_block_delta", {
			type: "content_block_delta",
			index: 0,
			delta: { type: "text_delta", text: `fixture response turn ${turn}` },
		});
		add("content_block_stop", { type: "content_block_stop", index: 0 });
	} else {
		let index = 0;
		if (kind === "signed-reasoning") {
			add("content_block_start", {
				type: "content_block_start",
				index,
				content_block: { type: "thinking", thinking: "", signature: "" },
			});
			add("content_block_delta", {
				type: "content_block_delta",
				index,
				delta: { type: "thinking_delta", thinking: "Inspect the requested fixture." },
			});
			add("content_block_delta", {
				type: "content_block_delta",
				index,
				delta: { type: "signature_delta", signature: "signed-fixture-reasoning" },
			});
			add("content_block_stop", { type: "content_block_stop", index });
			index += 1;
		} else if (kind === "redacted-reasoning") {
			add("content_block_start", {
				type: "content_block_start",
				index,
				content_block: { type: "redacted_thinking", data: "redacted-fixture-reasoning" },
			});
			add("content_block_stop", { type: "content_block_stop", index });
			index += 1;
		}

		const paths = kind === "multiple-tools" ? ["Cargo.toml", "README.md"] : ["Cargo.toml"];
		for (const [offset, path] of paths.entries()) {
			const callId = offset === 0 ? TOOL_CALL_1 : TOOL_CALL_2;
			add("content_block_start", {
				type: "content_block_start",
				index: index + offset,
				content_block: { type: "tool_use", id: callId, name: "read_file", input: {} },
			});
			add("content_block_delta", {
				type: "content_block_delta",
				index: index + offset,
				delta: { type: "input_json_delta", partial_json: JSON.stringify({ path }) },
			});
			add("content_block_stop", { type: "content_block_stop", index: index + offset });
		}
	}

	add("message_delta", {
		type: "message_delta",
		delta: { stop_reason: kind === "text" ? "end_turn" : "tool_use", stop_sequence: null },
		usage: {
			input_tokens: 12,
			output_tokens: 8,
			cache_read_input_tokens: 0,
			cache_creation_input_tokens: 0,
			output_tokens_details: { thinking_tokens: kind.includes("reasoning") ? 3 : 0 },
		},
	});
	add("message_stop", { type: "message_stop" });
	return events.map(({ event, data }) => `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`).join("");
}

function responsesFrames(
	family: "openai-responses" | "openai-codex-responses",
	kind: ResponseKind,
	model: string,
	turn: number,
): string {
	const responseId = `resp_fixture_${turn}`;
	const events: Record<string, unknown>[] = [
		{
			type: "response.created",
			response: { id: responseId, model, status: "in_progress", output: [] },
		},
	];
	const output: Record<string, unknown>[] = [];
	let outputIndex = 0;
	if (kind === "text") {
		const item = {
			type: "message",
			role: "assistant",
			content: [
				{
					type: "output_text",
					text: `fixture response turn ${turn}`,
					annotations: [],
				},
			],
			status: "completed",
			id: `msg_fixture_${turn}`,
			phase: "final_answer",
		};
		events.push({
			type: "response.output_item.added",
			output_index: outputIndex,
			item: { ...item, content: [], status: "in_progress" },
		});
		events.push({
			type: "response.output_text.delta",
			output_index: outputIndex,
			delta: `fixture response turn ${turn}`,
		});
		events.push({ type: "response.output_item.done", output_index: outputIndex, item });
		output.push(item);
	} else {
		if (kind === "signed-reasoning" || kind === "redacted-reasoning") {
			const reasoning = {
				id: `rs_fixture_${turn}`,
				type: "reasoning",
				summary: [{ type: "summary_text", text: "Inspect the requested fixture." }],
				content: [],
				encrypted_content:
					kind === "signed-reasoning"
						? "signed-fixture-reasoning"
						: "encrypted-fixture-reasoning",
				status: "completed",
			};
			events.push({
				type: "response.output_item.added",
				output_index: outputIndex,
				item: { id: reasoning.id, type: "reasoning", summary: [] },
			});
			events.push({
				type: "response.reasoning_summary_text.delta",
				output_index: outputIndex,
				delta: "Inspect the requested fixture.",
			});
			events.push({ type: "response.output_item.done", output_index: outputIndex, item: reasoning });
			output.push(reasoning);
			outputIndex += 1;
		}

		const paths = kind === "multiple-tools" ? ["Cargo.toml", "README.md"] : ["Cargo.toml"];
		for (const [offset, path] of paths.entries()) {
			const callId = offset === 0 ? TOOL_CALL_1 : TOOL_CALL_2;
			const itemId = `fc_fixture_${turn}_${offset + 1}`;
			const argumentsJson = JSON.stringify({ path });
			const item = {
				type: "function_call",
				id: itemId,
				call_id: callId,
				name: "read_file",
				arguments: argumentsJson,
				status: "completed",
			};
			events.push({
				type: "response.output_item.added",
				output_index: outputIndex + offset,
				item: { ...item, arguments: "", status: "in_progress" },
			});
			events.push({
				type: "response.function_call_arguments.delta",
				output_index: outputIndex + offset,
				delta: argumentsJson,
			});
			events.push({
				type: "response.function_call_arguments.done",
				output_index: outputIndex + offset,
				arguments: argumentsJson,
			});
			events.push({
				type: "response.output_item.done",
				output_index: outputIndex + offset,
				item,
			});
			output.push(item);
		}
	}

	events.push({
		type: family === "openai-codex-responses" ? "response.done" : "response.completed",
		response: {
			id: responseId,
			model,
			status: "completed",
			output,
			usage: {
				input_tokens: 12,
				output_tokens: 8,
				total_tokens: 20,
				input_tokens_details: { cached_tokens: 0 },
				output_tokens_details: { reasoning_tokens: kind.includes("reasoning") ? 3 : 0 },
			},
		},
	});
	return events.map((value) => `data: ${JSON.stringify(value)}\n\n`).join("");
}

function responseFrames(family: Family, kind: ResponseKind, model: string, turn: number): string {
	if (family === "openai-completions") return openAiFrames(kind, model, turn);
	if (family === "anthropic-messages") return anthropicFrames(kind, model, turn);
	return responsesFrames(family, kind, model, turn);
}

const SECRET_HEADER_NAMES = new Set([
	"authorization",
	"proxy-authorization",
	"x-api-key",
	"api-key",
	"cf-aig-authorization",
	"cookie",
	"set-cookie",
]);

const SEMANTIC_HEADER_NAMES = new Set([
	"accept",
	"content-type",
	"authorization",
	"x-api-key",
	"api-key",
	"cf-aig-authorization",
	"anthropic-beta",
	"anthropic-dangerous-direct-browser-access",
	"anthropic-version",
	"copilot-integration-id",
	"copilot-vision-request",
	"editor-plugin-version",
	"editor-version",
	"openai-intent",
	"openai-organization",
	"openai-project",
	"openai-beta",
	"chatgpt-account-id",
	"originator",
	"session-id",
	"session_id",
	"x-app",
	"x-client-request-id",
	"x-initiator",
	"x-session-affinity",
	"x-session-id",
]);

function captureHeaders(headers: Headers): {
	headers: Record<string, string>;
	omittedRuntimeHeaders: string[];
} {
	const result: Record<string, string> = {};
	const omittedRuntimeHeaders: string[] = [];
	for (const [name, value] of headers) {
		const lower = name.toLowerCase();
		if (!SEMANTIC_HEADER_NAMES.has(lower) && !lower.startsWith("x-fixture-")) {
			omittedRuntimeHeaders.push(lower);
			continue;
		}
		result[lower] = SECRET_HEADER_NAMES.has(lower) ? "[REDACTED]" : value;
	}
	omittedRuntimeHeaders.sort();
	return { headers: result, omittedRuntimeHeaders };
}

function proxyUrl(upstream: ProxyUpstream, requestUrl: URL): URL {
	if (!requestUrl.pathname.startsWith(upstream.localBasePath)) {
		throw new Error(
			`request path ${requestUrl.pathname} does not start with local base path ${upstream.localBasePath}`,
		);
	}
	const suffix = requestUrl.pathname.slice(upstream.localBasePath.length);
	const target = new URL(`${upstream.baseUrl.replace(/\/$/, "")}${suffix}`);
	target.search = requestUrl.search;
	return target;
}

function forwardedHeaders(source: Headers): Headers {
	const headers = new Headers(source);
	for (const name of ["host", "content-length", "connection", "accept-encoding"]) {
		headers.delete(name);
	}
	return headers;
}

function forwardedResponseHeaders(source: Headers): Headers {
	const headers = new Headers(source);
	for (const name of ["content-encoding", "content-length", "connection", "transfer-encoding"]) {
		headers.delete(name);
	}
	return headers;
}

function startCaptureServer(state: CaptureServerState): ReturnType<typeof Bun.serve> {
	return Bun.serve({
		port: 0,
		async fetch(request) {
			const requestUrl = new URL(request.url);
			const transportBody = new Uint8Array(await request.arrayBuffer());
			const body = request.headers.get("content-encoding") === "zstd"
				? Bun.zstdDecompressSync(transportBody)
				: transportBody;
			const capturedHeaders = captureHeaders(request.headers);
			state.requests.push({
				method: request.method,
				path: requestUrl.pathname,
				headers: capturedHeaders.headers,
				omittedRuntimeHeaders: capturedHeaders.omittedRuntimeHeaders,
				body,
			});

			if (state.upstream) {
				const response = await fetch(proxyUrl(state.upstream, requestUrl), {
					method: request.method,
					headers: forwardedHeaders(request.headers),
					body,
					redirect: "manual",
				});
				const responseBody = new Uint8Array(await response.arrayBuffer());
				state.responseBodies.push(responseBody);
				return new Response(responseBody, {
					status: response.status,
					statusText: response.statusText,
					headers: forwardedResponseHeaders(response.headers),
				});
			}

			const responseBody = state.scriptedResponses.shift();
			if (responseBody === undefined) {
				return new Response("unexpected fixture request", { status: 500 });
			}
			state.responseBodies.push(responseBody);
			return new Response(responseBody, {
				status: 200,
				headers: { "content-type": "text/event-stream", "x-request-id": "request-fixture-0001" },
			});
		},
	});
}

function continuationFor(message: Record<string, unknown>): Record<string, unknown>[] {
	const content = Array.isArray(message.content) ? message.content : [];
	const calls = content.filter(
		(block): block is { type: "toolCall"; id: string } =>
			typeof block === "object" && block !== null && (block as { type?: string }).type === "toolCall",
	);
	if (calls.length > 0) {
		return calls.map((call) => toolResult(call.id));
	}
	return [user("Deterministic turn-two follow-up.")];
}

function serializableOptions(options: Record<string, unknown>): Record<string, unknown> {
	const copy = { ...options };
	delete copy.apiKey;
	delete copy.fetch;
	delete copy.client;
	if (typeof copy.headers === "object" && copy.headers !== null && !Array.isArray(copy.headers)) {
		copy.headers = Object.fromEntries(
			Object.entries(copy.headers as Record<string, unknown>).map(([name, value]) => [
				name,
				SECRET_HEADER_NAMES.has(name.toLowerCase()) ? "[REDACTED]" : value,
			]),
		);
	}
	return copy;
}

function canonicalModel(model: Record<string, unknown>): Record<string, unknown> {
	const baseUrl = String(model.baseUrl);
	return {
		...model,
		baseUrl: baseUrl.replace(/^http:\/\/127\.0\.0\.1:\d+/, "http://127.0.0.1:<injected-port>"),
	};
}

function sha256(bytes: Uint8Array): string {
	return new Bun.CryptoHasher("sha256").update(bytes).digest("hex");
}

function parseRequestBody(request: CapturedRequest): Record<string, unknown> {
	const parsed = JSON.parse(new TextDecoder().decode(request.body)) as unknown;
	if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
		throw new Error("captured provider request body is not a JSON object");
	}
	return parsed as Record<string, unknown>;
}

function assertSameJsonField(
	family: Family,
	fixture: FixtureCase,
	turnOne: Record<string, unknown>,
	turnTwo: Record<string, unknown>,
	field: string,
): void {
	const left = JSON.stringify(turnOne[field]);
	const right = JSON.stringify(turnTwo[field]);
	if (left !== right) {
		throw new Error(
			`${family}/${fixture.name} lost simple lowering for ${field} on turn two: ${left} != ${right}`,
		);
	}
}

function assertRetainedTurnTwoLowering(
	family: Family,
	fixture: FixtureCase,
	requestOne: CapturedRequest,
	requestTwo: CapturedRequest,
): void {
	if (!fixture.simple) return;
	const turnOne = parseRequestBody(requestOne);
	const turnTwo = parseRequestBody(requestTwo);

	const maxTokenField = family === "anthropic-messages"
		? "max_tokens"
		: family === "openai-responses"
			? "max_output_tokens"
			: Object.hasOwn(turnOne, "max_tokens")
				? "max_tokens"
				: family === "openai-completions"
					? "max_completion_tokens"
					: undefined;
	if (maxTokenField === undefined) {
		for (const field of ["temperature", "service_tier", "reasoning", "text"]) {
			if (Object.hasOwn(turnOne, field)) assertSameJsonField(family, fixture, turnOne, turnTwo, field);
		}
		return;
	}
	const turnOneMax = turnOne[maxTokenField];
	const turnTwoMax = turnTwo[maxTokenField];
	if (typeof turnOneMax !== "number" || typeof turnTwoMax !== "number" || turnOneMax < 1 || turnTwoMax < 1) {
		throw new Error(`${family}/${fixture.name} did not retain a positive ${maxTokenField} on turn two`);
	}
	if (fixture.name === "max-output-clamp") {
		const requested = fixture.options?.maxTokens;
		if (typeof requested !== "number" || turnOneMax >= requested || turnTwoMax >= requested) {
			throw new Error(`${family}/${fixture.name} bypassed streamSimple max-output clamping on turn two`);
		}
	} else {
		assertSameJsonField(family, fixture, turnOne, turnTwo, maxTokenField);
	}

	if (isOpenAiFamily(family)) {
		for (const field of [
			"temperature",
			"top_p",
			"top_k",
			"seed",
			"reasoning_effort",
			"reasoning",
			"thinking",
			"enable_thinking",
			"chat_template_kwargs",
			"chat_template_args",
			"service_tier",
			"reasoning",
			"text",
		]) {
			if (Object.hasOwn(turnOne, field)) assertSameJsonField(family, fixture, turnOne, turnTwo, field);
		}
		if (fixture.options?.reasoning && !Object.hasOwn(turnTwo, "reasoning_effort") && !Object.hasOwn(turnTwo, "reasoning")) {
			throw new Error(`${family}/${fixture.name} dropped requested reasoning on turn two`);
		}
	} else {
		for (const field of ["temperature", "thinking", "output_config"]) {
			if (Object.hasOwn(turnOne, field)) assertSameJsonField(family, fixture, turnOne, turnTwo, field);
		}
		if (fixture.options?.reasoning && !Object.hasOwn(turnTwo, "thinking")) {
			throw new Error(`${family}/${fixture.name} dropped requested thinking on turn two`);
		}
	}

	if (fixture.name === "sampling-defaults-and-overrides") {
		for (const field of family === "openai-completions"
			? ["temperature", "top_p", "top_k", "seed"]
			: ["temperature"]) {
			if (!Object.hasOwn(turnTwo, field)) {
				throw new Error(`${family}/${fixture.name} dropped sampling field ${field} on turn two`);
			}
		}
	}
}

function assertCredentialResponseShape(family: Family, fixture: FixtureCase, turnOne: Record<string, unknown>): void {
	if (fixture.responseKind !== "tool" && fixture.responseKind !== "signed-reasoning") return;
	const content = Array.isArray(turnOne.content) ? turnOne.content : [];
	const toolCalls = content.filter(
		(block) => typeof block === "object" && block !== null && (block as { type?: string }).type === "toolCall",
	);
	if (toolCalls.length === 0) {
		throw new Error(`${family}/${fixture.name} credential-backed response did not contain the forced tool call`);
	}
	if (fixture.responseKind === "signed-reasoning") {
		const signedThinking = content.find((block) => {
			if (typeof block !== "object" || block === null || (block as { type?: string }).type !== "thinking") {
				return false;
			}
			const signature = (block as { thinkingSignature?: unknown }).thinkingSignature;
			return typeof signature === "string" && signature.length > 0;
		});
		if (!signedThinking) {
			throw new Error(`${family}/${fixture.name} credential-backed response had no replayable thinking signature`);
		}
	}
}

async function writeJson(path: string, value: unknown): Promise<void> {
	await Bun.write(path, `${JSON.stringify(value, null, 2)}\n`);
}

async function captureCase(
	family: Family,
	fixture: FixtureCase,
	apiModule: { stream: Function; streamSimple: Function },
	outputRoot: string,
	mode: CaptureMode,
	credentialTarget?: CredentialBackedTarget,
): Promise<void> {
	if (!/^[a-z0-9-]+$/.test(fixture.name)) throw new Error(`unsafe fixture name: ${fixture.name}`);
	if ((mode === "credential-backed") !== (credentialTarget !== undefined)) {
		throw new Error(`${family}/${fixture.name} credential target does not match capture mode`);
	}
	const state: CaptureServerState = {
		scriptedResponses: [],
		requests: [],
		responseBodies: [],
		upstream: credentialTarget?.upstream,
	};
	const server = startCaptureServer(state);
	try {
		const localBasePath = credentialTarget?.upstream.localBasePath ??
			(family === "openai-codex-responses" ? "" : "/v1");
		const baseUrl = `http://127.0.0.1:${server.port}${localBasePath}`;
		const model = modelFor(
			family,
			baseUrl,
			mergeNested(credentialTarget?.modelPatch ?? {}, fixture.modelPatch),
		);
		const modelId = String(model.id);
		const turnOneKind = fixture.responseKind ?? "text";
		if (mode === "hermetic") {
			state.scriptedResponses.push(new TextEncoder().encode(responseFrames(family, turnOneKind, modelId, 1)));
			state.scriptedResponses.push(new TextEncoder().encode(responseFrames(family, "text", modelId, 2)));
		}

		const fixtureHeaders = fixture.options?.headers as Record<string, string> | undefined;
		const options = {
			...fixture.options,
			...(fixtureHeaders || credentialTarget?.optionHeaders
				? { headers: { ...credentialTarget?.optionHeaders, ...fixtureHeaders } }
				: {}),
			...(mode === "hermetic"
				? { apiKey: family === "openai-codex-responses" ? FIXTURE_CODEX_API_KEY : FIXTURE_API_KEY }
				: credentialTarget?.apiKey
					? { apiKey: credentialTarget.apiKey }
					: {}),
			maxRetries: 0,
			timeoutMs: mode === "credential-backed" ? 60_000 : 10_000,
			...(family === "openai-codex-responses" ? { transport: "sse" } : {}),
		};
		const streamFunction = fixture.simple ? apiModule.streamSimple : apiModule.stream;
		const turnOne = await streamFunction(model, structuredClone(fixture.context), options).result();
		if (turnOne.stopReason === "error" || turnOne.stopReason === "aborted") {
			throw new Error(`${family}/${fixture.name} turn one failed: ${turnOne.errorMessage}`);
		}
		if (mode === "credential-backed") {
			assertCredentialResponseShape(family, fixture, turnOne);
		}
		const continuation = continuationFor(turnOne);
		const turnTwoContext = structuredClone(fixture.context) as { messages: unknown[] };
		turnTwoContext.messages.push(turnOne, ...continuation);
		const turnTwo = await streamFunction(model, turnTwoContext, options).result();
		if (turnTwo.stopReason === "error" || turnTwo.stopReason === "aborted") {
			throw new Error(`${family}/${fixture.name} turn two failed: ${turnTwo.errorMessage}`);
		}
		if (
			state.requests.length !== 2 ||
			state.responseBodies.length !== 2 ||
			(mode === "hermetic" && state.scriptedResponses.length !== 0)
		) {
			throw new Error(`${family}/${fixture.name} expected exactly two captured requests`);
		}
		assertRetainedTurnTwoLowering(family, fixture, state.requests[0], state.requests[1]);

		const directory = resolve(outputRoot, family, fixture.name);
		if (!directory.startsWith(resolve(outputRoot, family) + "/")) {
			throw new Error(`fixture directory escaped family root: ${directory}`);
		}
		rmSync(directory, { recursive: true, force: true });
		mkdirSync(directory, { recursive: true });
		const [requestOne, requestTwo] = state.requests;
		const responseOne = state.responseBodies[0];
		const canonical = {
			schemaVersion: 1,
			family,
			case: fixture.name,
			description: fixture.description,
			piCommit: PINNED_COMMIT,
			model: canonicalModel(model),
			context: fixture.context,
			options: serializableOptions(options),
			entrypoint: fixture.simple ? "streamSimple" : "stream",
			turnTwoAppend: continuation,
			deterministicInjections: {
				timestamp: FIXTURE_TIMESTAMP,
				sessionId: FIXTURE_SESSION_ID,
				...(mode === "hermetic"
					? { requestId: "request-fixture-0001", toolCallIds: [TOOL_CALL_1, TOOL_CALL_2] }
					: {}),
			},
			...(mode === "credential-backed" ? { providerGeneratedValuesCapturedVerbatim: true } : {}),
		};
		await writeJson(join(directory, "canonical.json"), canonical);
		await Bun.write(join(directory, "request-turn-1.body.json"), requestOne.body);
		await writeJson(join(directory, "request-turn-1.headers.json"), {
			schemaVersion: 1,
			method: requestOne.method,
			path: requestOne.path,
			headers: requestOne.headers,
			omittedRuntimeHeaders: requestOne.omittedRuntimeHeaders,
		});
		await Bun.write(join(directory, "response-turn-1.sse"), responseOne);
		await Bun.write(join(directory, "request-turn-2.body.json"), requestTwo.body);
		await writeJson(join(directory, "request-turn-2.headers.json"), {
			schemaVersion: 1,
			method: requestTwo.method,
			path: requestTwo.path,
			headers: requestTwo.headers,
			omittedRuntimeHeaders: requestTwo.omittedRuntimeHeaders,
		});
		await writeJson(join(directory, "metadata.json"), {
			schemaVersion: 1,
			captureMode: mode === "hermetic" ? "hermetic-local-server" : "credential-backed-local-proxy",
			credentialsUsed: mode === "credential-backed",
			credentialSource: credentialTarget?.credentialSource,
			secretsRedacted: true,
			requestTurnOneSha256: sha256(requestOne.body),
			responseTurnOneSha256: sha256(responseOne),
			requestTurnTwoSha256: sha256(requestTwo.body),
		});
	} finally {
		server.stop(true);
	}
}

function verifyPin(piRoot: string): void {
	const gitFile = join(piRoot, ".git");
	if (!existsSync(gitFile)) throw new Error(`PI checkout has no .git metadata: ${piRoot}`);
	const gitDirectory = statSync(gitFile).isDirectory()
		? gitFile
		: resolve(dirname(gitFile), readFileSync(gitFile, "utf8").trim().replace(/^gitdir:\s*/, ""));
	const head = readFileSync(join(gitDirectory, "HEAD"), "utf8").trim();
	let revision = head;
	if (head.startsWith("ref: ")) {
		revision = readFileSync(join(gitDirectory, head.slice(5)), "utf8").trim();
	}
	if (revision !== PINNED_COMMIT) {
		throw new Error(`Pi checkout must be ${PINNED_COMMIT}, found ${revision}`);
	}
}

function captureModeFromEnvironment(): CaptureMode {
	const value = process.env.PI_FIXTURE_CAPTURE_MODE ?? "hermetic";
	if (value === "hermetic" || value === "credential-backed") return value;
	throw new Error(`unsupported PI_FIXTURE_CAPTURE_MODE: ${value}`);
}

function selectedValues(environmentName: string, defaults: readonly string[]): Set<string> {
	const configured = process.env[environmentName];
	return new Set(
		(configured ? configured.split(",") : [...defaults])
			.map((value) => value.trim())
			.filter((value) => value.length > 0),
	);
}

function sanitizedCaptureError(error: unknown, credentials: readonly string[] = []): string {
	let message = error instanceof Error ? error.message : String(error);
	for (const credential of credentials) {
		message = message.replaceAll(credential, "[REDACTED]");
	}
	message = message
		.replace(/(authorization|x-api-key|api-key)\s*[:=]\s*[^\s,;]+/gi, "$1=[REDACTED]")
		.replace(/bearer\s+[^\s,;]+/gi, "Bearer [REDACTED]");
	return message.slice(0, 1000);
}

async function main(): Promise<void> {
	const toolRoot = import.meta.dir;
	const outputRoot = resolve(toolRoot, "..");
	const mode = captureModeFromEnvironment();
	const piRoot = resolve(process.env.PI_PIN_DIR ?? "/home/vikash/pi-pin-c49906ec7");
	verifyPin(piRoot);
	const workRoot = resolve(toolRoot, ".capture-work", "pi-ai");
	rmSync(workRoot, { recursive: true, force: true });
	mkdirSync(workRoot, { recursive: true });
	cpSync(join(piRoot, "packages", "ai", "src"), join(workRoot, "src"), { recursive: true });

	const openAiModule = await import(
		`${pathToFileURL(join(workRoot, "src", "api", "openai-completions.ts")).href}?pin=${PINNED_COMMIT}`
	);
	const anthropicModule = await import(
		`${pathToFileURL(join(workRoot, "src", "api", "anthropic-messages.ts")).href}?pin=${PINNED_COMMIT}`
	);
	const openAiResponsesModule = await import(
		`${pathToFileURL(join(workRoot, "src", "api", "openai-responses.ts")).href}?pin=${PINNED_COMMIT}`
	);
	const openAiCodexResponsesModule = await import(
		`${pathToFileURL(join(workRoot, "src", "api", "openai-codex-responses.ts")).href}?pin=${PINNED_COMMIT}`
	);
	const families: Array<{
		family: Family;
		provider: string;
		model: string;
		module: { stream: Function; streamSimple: Function };
	}> = [
		{
			family: "openai-completions",
			provider: "fixture-openai",
			model: "fixture-openai-model",
			module: openAiModule,
		},
		{
			family: "anthropic-messages",
			provider: "fixture-anthropic",
			model: "fixture-anthropic-model",
			module: anthropicModule,
		},
		{
			family: "openai-responses",
			provider: "fixture-openai",
			model: "fixture-openai-model",
			module: openAiResponsesModule,
		},
		{
			family: "openai-codex-responses",
			provider: "openai-codex",
			model: "fixture-codex-model",
			module: openAiCodexResponsesModule,
		},
	];

	const defaultCredentialCases = ["text-only", "one-tool-call", "tool-results", "signed-thinking-replay"];
	const selectedCases = selectedValues(
		"PI_FIXTURE_CASES",
		mode === "credential-backed" ? defaultCredentialCases : commonCases("openai-completions", "", "").map((item) => item.name),
	);
	const selectedFamilies = selectedValues("PI_FIXTURE_FAMILIES", families.map((item) => item.family));
	const credentialResults: CredentialCaptureResult[] = [];
	const authStore = mode === "credential-backed" ? readPiAuthStore() : {};
	const captureOutputRoot = mode === "credential-backed" ? resolve(outputRoot, "credential-backed") : outputRoot;

	try {
		for (const family of families) {
			if (!selectedFamilies.has(family.family)) continue;
			const fixtures = commonCases(family.family, family.provider, family.model)
				.filter((fixture) => selectedCases.has(fixture.name));
			let credentialTarget: CredentialBackedTarget | undefined;
			if (mode === "credential-backed") {
				try {
					credentialTarget = await credentialTargetFor(family.family, authStore);
				} catch (error) {
					const reason = sanitizedCaptureError(error);
					for (const fixture of fixtures) {
						credentialResults.push({
							family: family.family,
							case: fixture.name,
							status: "not-captured",
							reason,
						});
					}
					continue;
				}
			}

			for (const originalFixture of fixtures) {
				const fixture = mode === "credential-backed"
					? withCredentialToolChoice(family.family, originalFixture)
					: originalFixture;
				try {
					await captureCase(
						family.family,
						fixture,
						family.module,
						captureOutputRoot,
						mode,
						credentialTarget,
					);
					console.log(`captured ${family.family}/${fixture.name} (${mode})`);
					if (mode === "credential-backed") {
						credentialResults.push({ family: family.family, case: fixture.name, status: "captured" });
					}
				} catch (error) {
					if (mode !== "credential-backed") throw error;
					const reason = sanitizedCaptureError(error, credentialTarget?.secretValues ?? []);
					console.warn(`not captured ${family.family}/${fixture.name}: ${reason}`);
					credentialResults.push({
						family: family.family,
						case: fixture.name,
						status: "not-captured",
						reason,
					});
				}
			}
		}

		if (mode === "credential-backed") {
			mkdirSync(captureOutputRoot, { recursive: true });
			await writeJson(join(captureOutputRoot, "report.json"), {
				schemaVersion: 1,
				piCommit: PINNED_COMMIT,
				captureMode: "credential-backed-local-proxy",
				results: credentialResults,
			});
		}
	} finally {
		rmSync(workRoot, { recursive: true, force: true });
	}
}

await main();
