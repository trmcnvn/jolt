//! Byte-bounded transcript projection for tail-first viewports.
//!
//! Page IDs are anchored to stable message IDs. Canonical runtime projection is
//! generated directly from normalized SQLite rows in `jolt-store`.

use serde::{Deserialize, Serialize};

use crate::{MessagePart, MessageRole, SessionMessageEntry};

/// Logical entries per ordinary page. The byte ceiling wins when reached first.
pub const TRANSCRIPT_PAGE_MESSAGE_COUNT: usize = 32;
/// Soft serialized payload target. A single logical message may exceed it.
pub const TRANSCRIPT_PAGE_TARGET_BYTES: usize = 384 * 1024;
/// Opening bootstrap includes enough sealed pages to cover at least this many messages.
pub const TRANSCRIPT_BOOTSTRAP_MESSAGE_COUNT: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptPageDescriptor {
    pub id: String,
    pub revision: String,
    /// SHA-256 of the serialized page object when prepared for immutable edge storage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    pub first_ordinal: usize,
    pub message_count: usize,
    pub estimated_bytes: usize,
    pub previous_page_id: Option<String>,
    pub next_page_id: Option<String>,
    pub live: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptTurnDescriptor {
    pub message_id: String,
    pub ordinal: usize,
    pub page_id: String,
    pub prompt_preview: String,
    pub reply_preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptManifest {
    pub catalog_revision: String,
    pub total_messages: usize,
    pub pages: Vec<TranscriptPageDescriptor>,
    pub turns: Vec<TranscriptTurnDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptPage {
    pub id: String,
    pub revision: String,
    pub first_ordinal: usize,
    pub messages: Vec<SessionMessageEntry>,
}

/// One server-side transcript search hit. The page anchor lets bounded clients
/// materialize only the selected result instead of loading the whole session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptSearchResult {
    pub message_id: String,
    pub page_id: String,
    pub ordinal: usize,
    pub role: MessageRole,
    pub preview: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptBootstrap {
    pub sequence: u64,
    pub manifest: TranscriptManifest,
    /// Consecutive trailing pages, oldest first.
    pub pages: Vec<TranscriptPage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TranscriptWatchFrame {
    Bootstrap {
        bootstrap: TranscriptBootstrap,
    },
    Delta {
        sequence: u64,
        page_id: String,
        page_revision: String,
        frame: crate::TranscriptFrame,
    },
}

/// Approximate serialized bytes used for page budgeting and local storage counters.
pub fn message_estimated_bytes(entry: &SessionMessageEntry) -> usize {
    entry.id.len()
        + entry.device_id.len()
        + entry.continuation_of.as_ref().map_or(0, String::len)
        + entry.parts.iter().map(MessagePart::byte_len).sum::<usize>()
        + 48
}

/// Stable content revision for one materialized transcript page.
pub fn transcript_page_revision(messages: &[SessionMessageEntry]) -> String {
    let mut hash = Hash64::new();
    for entry in messages {
        match serde_json::to_vec(entry) {
            Ok(bytes) => hash.write(&bytes),
            Err(_) => {
                // SessionMessageEntry's concrete fields are serializable; keep
                // a deterministic fallback rather than making catalog builds
                // fail if that invariant changes in a newer schema.
                hash.write(entry.id.as_bytes());
                hash.write_u64(entry.created_at as u64);
                hash.write_u64(message_estimated_bytes(entry) as u64);
            }
        }
    }
    hash.finish_hex()
}

/// Compact prose preview used by transcript manifests.
pub fn transcript_entry_preview(entry: &SessionMessageEntry, limit: usize) -> String {
    let text = entry
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    truncate_text(&text, limit)
}

/// Render-safe searchable text extracted from one transcript entry.
pub fn transcript_searchable_text(entry: &SessionMessageEntry) -> String {
    let mut fragments = Vec::new();
    for part in &entry.parts {
        match part {
            MessagePart::Text { text, .. } => fragments.push(text.clone()),
            MessagePart::Error { message, .. } => fragments.push(message.clone()),
            MessagePart::Tool { call, .. } => {
                if let Ok(value) = serde_json::to_value(call) {
                    append_json_strings(&value, &mut fragments);
                }
            }
            MessagePart::Input { questions, .. } => {
                if let Ok(value) = serde_json::to_value(questions) {
                    append_json_strings(&value, &mut fragments);
                }
            }
            MessagePart::TextReveal { .. }
            | MessagePart::HarnessSwitch { .. }
            | MessagePart::Changes { .. } => {}
        }
    }
    fragments
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn append_json_strings(value: &serde_json::Value, strings: &mut Vec<String>) {
    match value {
        serde_json::Value::String(value) => strings.push(value.clone()),
        serde_json::Value::Array(values) => {
            for value in values {
                append_json_strings(value, strings);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values() {
                append_json_strings(value, strings);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

/// A bounded search result preview with the first match kept near the front.
pub fn transcript_search_preview(text: &str, terms: &[String], limit: usize) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    let matched = words.iter().position(|word| {
        let word = word.to_lowercase();
        terms.iter().any(|term| word.contains(term))
    });
    // Keep the first matching word near the front so the single-line result
    // row cannot truncate away the reason this message matched.
    let start = matched.map_or(0, |index| index.saturating_sub(3));
    let end = (start + 24).min(words.len());
    let mut preview = words[start..end].join(" ");
    if start > 0 {
        preview.insert_str(0, "… ");
    }
    if end < words.len() {
        preview.push_str(" …");
    }
    truncate_text(&preview, limit)
}

fn truncate_text(text: &str, limit: usize) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= limit {
        return flattened;
    }
    let mut out: String = flattened.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Stable catalog revision for a manifest's page layout and turn anchors.
pub fn transcript_catalog_revision(
    pages: &[TranscriptPageDescriptor],
    turns: &[TranscriptTurnDescriptor],
) -> String {
    let mut hash = Hash64::new();
    for page in pages {
        hash.write(page.id.as_bytes());
        hash.write_u64(page.first_ordinal as u64);
        hash.write_u64(page.message_count as u64);
    }
    for turn in turns {
        hash.write(turn.message_id.as_bytes());
        hash.write_u64(turn.ordinal as u64);
    }
    hash.finish_hex()
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Hash64(u64);

impl Hash64 {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x1_0000_01b3);
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.write(&value.to_le_bytes());
    }

    fn finish_hex(self) -> String {
        format!("{:016x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_preview_keeps_the_match_near_the_front() {
        let text = "one two three four five six seven eight nine distinctive Needle phrase after";
        let preview = transcript_search_preview(text, &["needle".into()], 240);
        assert!(preview.starts_with("… eight nine distinctive Needle"));
    }
}
