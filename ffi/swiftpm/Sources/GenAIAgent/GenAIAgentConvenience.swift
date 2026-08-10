//
//  GenAIAgentConvenience.swift
//  GenAIAgent
//
//  Hand-written ergonomic helpers over the UniFFI-generated bindings.
//  This file is NOT regenerated — it persists across `build_apple.sh` runs.
//  If the FFI surface changes, these inits fail to compile, forcing a
//  deliberate review (type-checked drift detection).

import Foundation

// MARK: - ChatOptions

extension ChatOptions {
	/// All fields default to genai's defaults (`nil` / empty) — set only what
	/// you need, e.g. `ChatOptions(captureUsage: true)`.
	///
	/// NOTE: parameter order intentionally differs from the memberwise init
	/// (capture flags first) — identical labels would be an invalid redeclaration.
	public init(
		captureUsage: Bool? = nil,
		captureContent: Bool? = nil,
		captureReasoningContent: Bool? = nil,
		captureToolCalls: Bool? = nil,
		captureRawBody: Bool? = nil,
		temperature: Double? = nil,
		maxTokens: UInt32? = nil,
		topP: Double? = nil,
		stopSequences: [String] = [],
		responseFormat: ChatResponseFormat? = nil,
		toolChoice: ToolChoice? = nil,
		reasoningEffort: ReasoningEffort? = nil,
		verbosity: Verbosity? = nil,
		normalizeReasoningContent: Bool? = nil,
		seed: UInt64? = nil,
		serviceTier: ServiceTier? = nil,
		cacheControl: CacheControl? = nil,
		promptCacheKey: String? = nil,
		extraHeaders: [String: String]? = nil,
		extraBodyJson: String? = nil
	) {
		self.init(
			temperature: temperature,
			maxTokens: maxTokens,
			topP: topP,
			stopSequences: stopSequences,
			captureUsage: captureUsage,
			captureContent: captureContent,
			captureReasoningContent: captureReasoningContent,
			captureToolCalls: captureToolCalls,
			captureRawBody: captureRawBody,
			responseFormat: responseFormat,
			toolChoice: toolChoice,
			normalizeReasoningContent: normalizeReasoningContent,
			reasoningEffort: reasoningEffort,
			verbosity: verbosity,
			seed: seed,
			serviceTier: serviceTier,
			extraHeaders: extraHeaders,
			cacheControl: cacheControl,
			promptCacheKey: promptCacheKey,
			extraBodyJson: extraBodyJson
		)
	}
}

// MARK: - AgentSetup

extension AgentSetup {
	/// Ergonomic defaults — set only what you need, e.g.
	/// `AgentSetup(model: "gpt-5-mini", systemPrompt: "You are concise")`.
	///
	/// NOTE: parameter order intentionally differs from the memberwise init
	/// (required fields first) — identical labels would be an invalid redeclaration.
	public init(
		model: String,
		systemPrompt: String = "",
		messages: [AgentMessage] = [],
		chatOptions: ChatOptions = ChatOptions(),
		thinkingLevel: ThinkingLevel = .off,
		sessionId: String? = nil,
		thinkingBudgets: ThinkingBudgets? = nil,
		maxRetries: UInt32? = nil,
		maxRetryDelayMs: UInt64? = nil,
		toolExecution: ToolExecutionMode = .parallel,
		transport: Transport = .auto,
		steeringMode: QueueMode = .all,
		followUpMode: QueueMode = .all
	) {
		self.init(
			systemPrompt: systemPrompt,
			model: model,
			sessionId: sessionId,
			messages: messages,
			thinkingLevel: thinkingLevel,
			thinkingBudgets: thinkingBudgets,
			maxRetries: maxRetries,
			maxRetryDelayMs: maxRetryDelayMs,
			toolExecution: toolExecution,
			transport: transport,
			steeringMode: steeringMode,
			followUpMode: followUpMode,
			chatOptions: chatOptions
		)
	}
}

// MARK: - AssistantMessage

extension AssistantMessage {
	/// The concatenated text parts of this message, if any.
	public var text: String {
		content.compactMap { part in
			if case .text(let text, _) = part { text } else { nil }
		}.joined()
	}

	/// The concatenated thinking parts of this message, if any.
	public var thinking: String {
		content.compactMap { part in
			if case .thinking(let thinking, _) = part { thinking } else { nil }
		}.joined()
	}
}

// MARK: - AgentUsage

extension AgentUsage {
	/// Compact display string, e.g. "in 120 · out 48 · total 168".
	public var summary: String {
		"in \(inputTokens) · out \(outputTokens) · total \(inputTokens + outputTokens)"
	}
}
