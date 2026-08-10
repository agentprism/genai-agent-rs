//
//  GenAIAgentConvenience.swift
//  GenAIAgent
//
//  Hand-written ergonomic helpers over the UniFFI-generated bindings.
//  This file is NOT regenerated — it persists across `build_apple.sh` runs.
//  If the FFI surface changes, these inits fail to compile, forcing a
//  deliberate review (type-checked drift detection).

import Foundation

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
		thinkingLevel: ThinkingLevel = .off,
		sessionId: String? = nil,
		thinkingBudgets: ThinkingBudgets? = nil,
		maxRetries: UInt32? = nil,
		maxRetryDelayMs: UInt64? = nil,
		toolExecution: ToolExecutionMode = .parallel,
		transport: Transport = .auto,
		steeringMode: QueueMode = .all,
		followUpMode: QueueMode = .all,
		initialMessagesJson: String? = nil
	) {
		self.init(
			systemPrompt: systemPrompt,
			model: model,
			sessionId: sessionId,
			thinkingLevel: thinkingLevel,
			thinkingBudgets: thinkingBudgets,
			maxRetries: maxRetries,
			maxRetryDelayMs: maxRetryDelayMs,
			toolExecution: toolExecution,
			transport: transport,
			steeringMode: steeringMode,
			followUpMode: followUpMode,
			initialMessagesJson: initialMessagesJson
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
		var parts = ["in \(inputTokens)", "out \(outputTokens)"]
		parts.append("total \(inputTokens + outputTokens)")
		return parts.joined(separator: " · ")
	}
}
