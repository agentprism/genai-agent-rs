//! Deferred-tool partitioning ⇐ pi `src/utils/deferred-tools.ts`.

use crate::types::{AssistantContent, Context, Message, Tool};
use indexmap::{IndexMap, IndexSet};

#[derive(Debug, Clone, PartialEq)]
pub struct SplitDeferredTools {
    pub immediate: Vec<Tool>,
    pub deferred: IndexMap<String, Tool>,
}

pub fn split_deferred_tools<F>(
    context: &Context,
    enabled: bool,
    normalize_name: F,
) -> SplitDeferredTools
where
    F: Fn(&str) -> String,
{
    let mut unique = IndexMap::new();
    for tool in context.tools.iter().flatten() {
        unique.insert(normalize_name(&tool.name), tool.clone());
    }
    if !enabled {
        return SplitDeferredTools {
            immediate: unique.into_values().collect(),
            deferred: IndexMap::new(),
        };
    }

    let mut deferred_names = IndexSet::new();
    let mut used_names = IndexSet::new();
    for message in &context.messages {
        match message {
            Message::Assistant(message) => {
                for block in &message.content {
                    if let AssistantContent::ToolCall(call) = block {
                        used_names.insert(normalize_name(&call.name));
                    }
                }
            }
            Message::ToolResult(message) => {
                for name in message.added_tool_names.iter().flatten() {
                    let name = normalize_name(name);
                    if !used_names.contains(&name) {
                        deferred_names.insert(name);
                    }
                }
            }
            Message::User(_) => {}
        }
    }

    let mut immediate = Vec::new();
    let mut deferred = IndexMap::new();
    for (name, tool) in unique {
        if deferred_names.contains(&name) {
            deferred.insert(name, tool);
        } else {
            immediate.push(tool);
        }
    }
    SplitDeferredTools {
        immediate,
        deferred,
    }
}

pub fn split_deferred_tools_identity(context: &Context, enabled: bool) -> SplitDeferredTools {
    split_deferred_tools(context, enabled, str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AssistantMessage, ToolCall, ToolResultMessage, ToolResultRole, UserContentBlock,
    };
    use serde_json::{Map, json};

    fn tool(name: &str, description: &str) -> Tool {
        Tool {
            name: name.to_owned(),
            description: description.to_owned(),
            parameters: json!({"type":"object"}),
            constrained_sampling: None,
        }
    }

    fn marker(names: &[&str]) -> Message {
        Message::ToolResult(Box::new(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "call".into(),
            tool_name: "base".into(),
            content: Vec::<UserContentBlock>::new(),
            details: None,
            usage: None,
            added_tool_names: Some(names.iter().map(|name| (*name).into()).collect()),
            is_error: false,
            timestamp: 2.0,
        }))
    }

    /// Pins the shared partition semantics exercised by pi
    /// `test/deferred-tools.test.ts:231-323`.
    #[test]
    fn preserves_order_usage_markers_normalization_and_last_definition() {
        let mut used = AssistantMessage::pending("api", "provider", "model", 1.0);
        used.content = vec![AssistantContent::ToolCall(ToolCall::new(
            "call",
            "Read",
            Map::new(),
        ))];
        let context = Context {
            system_prompt: None,
            messages: vec![
                Message::Assistant(Box::new(used)),
                marker(&["read", "late", "missing"]),
            ],
            tools: Some(vec![
                tool("base", "base"),
                tool("read", "old"),
                tool("Read", "canonical"),
                tool("late", "late"),
            ]),
        };
        let split = split_deferred_tools(&context, true, |name| name.to_ascii_lowercase());
        assert_eq!(
            split
                .immediate
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["base", "Read"]
        );
        assert_eq!(split.immediate[1].description, "canonical");
        assert_eq!(
            split
                .deferred
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["late"]
        );

        let disabled = split_deferred_tools_identity(&context, false);
        assert_eq!(
            disabled
                .immediate
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["base", "read", "Read", "late"]
        );
        assert!(disabled.deferred.is_empty());
    }
}
