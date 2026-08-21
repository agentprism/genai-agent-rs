//! Text extraction ⇐ pi `src/utils/text.ts`.

use crate::types::{AssistantContent, AssistantMessageContent, UserContent, UserContentBlock};

pub trait ContentText {
    fn append_text(&self, output: &mut Vec<String>);
}

impl ContentText for str {
    fn append_text(&self, output: &mut Vec<String>) {
        output.push(self.to_owned());
    }
}

impl ContentText for String {
    fn append_text(&self, output: &mut Vec<String>) {
        output.push(self.clone());
    }
}

impl ContentText for UserContent {
    fn append_text(&self, output: &mut Vec<String>) {
        match self {
            Self::Text(text) => output.push(text.clone()),
            Self::Blocks(blocks) => blocks.as_slice().append_text(output),
        }
    }
}

impl ContentText for [UserContentBlock] {
    fn append_text(&self, output: &mut Vec<String>) {
        output.extend(self.iter().filter_map(|block| match block {
            UserContentBlock::Text(text) => Some(text.text.clone()),
            UserContentBlock::Image(_) => None,
        }));
    }
}

impl ContentText for [AssistantContent] {
    fn append_text(&self, output: &mut Vec<String>) {
        output.extend(self.iter().filter_map(|block| match block {
            AssistantContent::Text(text) => Some(text.text.clone()),
            AssistantContent::Thinking(_) | AssistantContent::ToolCall(_) => None,
        }));
    }
}

impl ContentText for AssistantMessageContent {
    fn append_text(&self, output: &mut Vec<String>) {
        self.as_slice().append_text(output);
    }
}

pub fn content_text(content: &(impl ContentText + ?Sized), separator: Option<&str>) -> String {
    let mut text = Vec::new();
    content.append_text(&mut text);
    text.join(separator.unwrap_or("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ImageContent, TextContent};

    #[test]
    fn ports_text_cases() {
        assert_eq!(content_text(&"hello".to_owned(), None), "hello");
        let blocks = vec![
            UserContentBlock::Text(TextContent::new("one")),
            UserContentBlock::Image(ImageContent::new("data", "image/png")),
            UserContentBlock::Text(TextContent::new("two")),
        ];
        assert_eq!(content_text(blocks.as_slice(), None), "one\ntwo");
        assert_eq!(content_text(blocks.as_slice(), Some(" | ")), "one | two");

        let assistant = vec![
            AssistantContent::Thinking(crate::types::ThinkingContent::new("hidden")),
            AssistantContent::Text(TextContent::new("first")),
            AssistantContent::ToolCall(crate::types::ToolCall::new(
                "call",
                "run",
                serde_json::Map::new(),
            )),
            AssistantContent::Text(TextContent::new("second")),
        ];
        assert_eq!(content_text(assistant.as_slice(), None), "first\nsecond");
        assert_eq!(content_text(assistant.as_slice(), Some("")), "firstsecond");
    }
}
