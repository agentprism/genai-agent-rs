//! GitHub Copilot dynamic headers ⇐ pi `src/api/github-copilot-headers.ts`.

use crate::types::{Message, UserContent, UserContentBlock};
use std::collections::BTreeMap;

pub fn infer_copilot_initiator(messages: &[Message]) -> &'static str {
    match messages.last() {
        Some(Message::Assistant(_) | Message::ToolResult(_)) => "agent",
        Some(Message::User(_)) | None => "user",
    }
}

pub fn has_copilot_vision_input(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::User(message) => match &message.content {
            UserContent::Blocks(blocks) => blocks
                .iter()
                .any(|block| matches!(block, UserContentBlock::Image(_))),
            UserContent::Text(_) => false,
        },
        Message::ToolResult(message) => message
            .content
            .iter()
            .any(|block| matches!(block, UserContentBlock::Image(_))),
        Message::Assistant(_) => false,
    })
}

pub fn build_copilot_dynamic_headers(
    messages: &[Message],
    has_images: bool,
) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::from([
        (
            "X-Initiator".to_owned(),
            infer_copilot_initiator(messages).to_owned(),
        ),
        ("Openai-Intent".to_owned(), "conversation-edits".to_owned()),
    ]);
    if has_images {
        headers.insert("Copilot-Vision-Request".to_owned(), "true".to_owned());
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AssistantMessage, ImageContent, ToolResultMessage, ToolResultRole, UserMessage, UserRole,
    };

    /// Derived from pi `src/api/github-copilot-headers.ts:3-37`.
    #[test]
    fn infers_initiator_vision_and_dynamic_headers() {
        assert_eq!(infer_copilot_initiator(&[]), "user");
        let user = Message::User(Box::new(UserMessage {
            role: UserRole::User,
            content: UserContent::Blocks(vec![UserContentBlock::Image(ImageContent::new(
                "AA==",
                "image/png",
            ))]),
            timestamp: 1.0,
        }));
        assert_eq!(infer_copilot_initiator(std::slice::from_ref(&user)), "user");
        assert!(has_copilot_vision_input(std::slice::from_ref(&user)));

        let assistant = Message::Assistant(Box::new(AssistantMessage::pending(
            "openai-responses",
            "github-copilot",
            "gpt",
            2.0,
        )));
        assert_eq!(infer_copilot_initiator(&[user.clone(), assistant]), "agent");

        let tool_result = Message::ToolResult(Box::new(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "call".into(),
            tool_name: "image".into(),
            content: vec![UserContentBlock::Image(ImageContent::new(
                "AA==",
                "image/png",
            ))],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: 3.0,
        }));
        assert!(has_copilot_vision_input(std::slice::from_ref(&tool_result)));
        let headers = build_copilot_dynamic_headers(&[user, tool_result], true);
        assert_eq!(
            headers.get("X-Initiator").map(String::as_str),
            Some("agent")
        );
        assert_eq!(
            headers.get("Openai-Intent").map(String::as_str),
            Some("conversation-edits")
        );
        assert_eq!(
            headers.get("Copilot-Vision-Request").map(String::as_str),
            Some("true")
        );
        assert!(!build_copilot_dynamic_headers(&[], false).contains_key("Copilot-Vision-Request"));
    }
}
