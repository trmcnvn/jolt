//! Render-safe session message model shared by storage, sync, and viewports.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{MessagePart, MessageStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMessageEntry {
    pub id: String,
    pub role: MessageRole,
    pub parts: Vec<MessagePart>,
    /// Epoch milliseconds.
    pub created_at: i64,
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<MessageStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_of: Option<String>,
}

/// Join imported continuation records into their root messages.
pub fn join_continuation_entries(entries: Vec<SessionMessageEntry>) -> Vec<SessionMessageEntry> {
    if !entries.iter().any(|entry| entry.continuation_of.is_some()) {
        return entries;
    }
    let mut joined: Vec<SessionMessageEntry> = Vec::with_capacity(entries.len());
    let mut root_indexes: HashMap<String, usize> = HashMap::new();
    for entry in entries {
        match &entry.continuation_of {
            Some(root_id) => {
                if let Some(&index) = root_indexes.get(root_id) {
                    joined[index].parts.extend(entry.parts);
                } else {
                    // Preserve orphaned continuations rather than dropping visible data.
                    joined.push(entry);
                }
            }
            None => {
                root_indexes.insert(entry.id.clone(), joined.len());
                joined.push(entry);
            }
        }
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, continuation_of: Option<&str>) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                id: format!("part-{id}"),
                text: id.into(),
            }],
            created_at: 0,
            device_id: "device".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: continuation_of.map(str::to_string),
        }
    }

    #[test]
    fn joins_continuations_and_preserves_orphans() {
        let joined = join_continuation_entries(vec![
            message("root", None),
            message("continuation", Some("root")),
            message("orphan", Some("missing")),
        ]);
        assert_eq!(joined.len(), 2);
        assert_eq!(joined[0].parts.len(), 2);
        assert_eq!(joined[1].id, "orphan");
    }
}
