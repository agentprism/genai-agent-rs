//! File-operation metadata shared by compaction and branch summaries.

use agentprism_ai::{ContentBlock, Message, VersionedExtension};
use agentprism_core::AgentRecord;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Default)]
pub(crate) struct FileOperations {
    read: BTreeSet<String>,
    written: BTreeSet<String>,
    edited: BTreeSet<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct FileOperationDetails {
    read_files: Vec<String>,
    modified_files: Vec<String>,
}

impl FileOperations {
    pub(crate) fn extract_record(&mut self, record: &AgentRecord) {
        let AgentRecord::Llm(Message::Assistant(message)) = record else {
            return;
        };
        for block in &message.content {
            let ContentBlock::ToolCall { call, .. } = block else {
                continue;
            };
            let Some(path) = call.arguments.get("path").and_then(|value| value.as_str()) else {
                continue;
            };
            match call.name.as_str() {
                "read" => {
                    self.read.insert(path.to_owned());
                }
                "write" => {
                    self.written.insert(path.to_owned());
                }
                "edit" => {
                    self.edited.insert(path.to_owned());
                }
                _ => {}
            }
        }
    }

    pub(crate) fn extend_details(&mut self, details: Option<&VersionedExtension>) {
        let Some(details) = details else {
            return;
        };
        let Ok(details) = serde_json::from_str::<FileOperationDetails>(details.value.get()) else {
            return;
        };
        self.read.extend(details.read_files);
        self.edited.extend(details.modified_files);
    }

    pub(crate) fn lists(&self) -> (Vec<String>, Vec<String>) {
        let modified = self
            .edited
            .union(&self.written)
            .cloned()
            .collect::<BTreeSet<_>>();
        let read = self.read.difference(&modified).cloned().collect();
        (read, modified.into_iter().collect())
    }
}

pub(crate) fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read_files.join("\n")
        ));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", sections.join("\n\n"))
    }
}

pub(crate) fn file_operation_details(
    read_files: Vec<String>,
    modified_files: Vec<String>,
) -> VersionedExtension {
    let json = serde_json::to_string(&FileOperationDetails {
        read_files,
        modified_files,
    })
    .expect("file-operation details are always JSON serializable");
    VersionedExtension {
        schema_version: 1,
        value: RawValue::from_string(json)
            .expect("serialized file-operation details are valid JSON"),
    }
}
