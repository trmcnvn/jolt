//! Byte-bounded transcript projection for tail-first viewports.
//!
//! The Loro document remains authoritative. This module builds a compact
//! catalog and materializes individual pages without retaining a second full
//! transcript vector. Page ids are anchored to stable message ids; list
//! ordinals are descriptive only and may change after a CRDT merge.

use serde::{Deserialize, Serialize};

use crate::{
    DocError, MessagePart, MessageRole, SessionDoc, SessionMessageEntry, join_continuation_entries,
};

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

#[derive(Debug, Clone)]
struct PageSlot {
    descriptor: TranscriptPageDescriptor,
    physical_start: usize,
    physical_end: usize,
}

/// Compact catalog plus physical Loro ranges used to fetch pages on demand.
#[derive(Debug, Clone)]
pub struct TranscriptCatalog {
    manifest: TranscriptManifest,
    slots: Vec<PageSlot>,
}

impl TranscriptCatalog {
    pub fn build(doc: &SessionDoc) -> Result<Self, DocError> {
        let physical_len = doc.message_count();
        let mut logical = Vec::new();
        let mut slots = Vec::new();
        let mut page_start = 0usize;
        let mut ordinal = 0usize;
        let mut catalog_hash = Hash64::new();

        for physical_index in 0..physical_len {
            let Some(entry) = doc.read_entry_at(physical_index)? else {
                continue;
            };
            let is_continuation = entry.continuation_of.is_some();
            if !is_continuation
                && !logical.is_empty()
                && (logical.len() >= TRANSCRIPT_PAGE_MESSAGE_COUNT
                    || page_bytes(&logical) + message_bytes(&entry) > TRANSCRIPT_PAGE_TARGET_BYTES)
            {
                push_slot(&mut slots, page_start, physical_index, ordinal, &logical);
                ordinal += logical.len();
                logical.clear();
                page_start = physical_index;
            }
            append_joined(&mut logical, entry);
        }
        if !logical.is_empty() {
            push_slot(&mut slots, page_start, physical_len, ordinal, &logical);
        }

        let slot_count = slots.len();
        for index in 0..slot_count {
            let previous = index.checked_sub(1).map(|i| slots[i].descriptor.id.clone());
            let next = slots.get(index + 1).map(|slot| slot.descriptor.id.clone());
            let slot = &mut slots[index];
            slot.descriptor.previous_page_id = previous;
            slot.descriptor.next_page_id = next;
            slot.descriptor.live = index + 1 == slot_count;
            catalog_hash.write(slot.descriptor.id.as_bytes());
            catalog_hash.write_u64(slot.descriptor.first_ordinal as u64);
            catalog_hash.write_u64(slot.descriptor.message_count as u64);
        }

        let total_messages = slots.last().map_or(0, |slot| {
            slot.descriptor.first_ordinal + slot.descriptor.message_count
        });
        let mut turns = Vec::new();
        for slot in &slots {
            let entries = read_slot(doc, slot)?;
            for (offset, entry) in entries.iter().enumerate() {
                let absolute = slot.descriptor.first_ordinal + offset;
                if entry.role == MessageRole::User {
                    turns.push(TranscriptTurnDescriptor {
                        message_id: entry.id.clone(),
                        ordinal: absolute,
                        page_id: slot.descriptor.id.clone(),
                        prompt_preview: preview(entry, 160),
                        reply_preview: None,
                    });
                } else if entry.role == MessageRole::Assistant
                    && let Some(turn) = turns.last_mut()
                    && turn.reply_preview.is_none()
                {
                    let text = preview(entry, 200);
                    if !text.is_empty() {
                        turn.reply_preview = Some(text);
                    }
                }
            }
        }
        for turn in &turns {
            catalog_hash.write(turn.message_id.as_bytes());
            catalog_hash.write_u64(turn.ordinal as u64);
        }

        Ok(Self {
            manifest: TranscriptManifest {
                catalog_revision: catalog_hash.finish_hex(),
                total_messages,
                pages: slots.iter().map(|slot| slot.descriptor.clone()).collect(),
                turns,
            },
            slots,
        })
    }

    pub fn manifest(&self) -> &TranscriptManifest {
        &self.manifest
    }

    pub fn page(
        &self,
        doc: &SessionDoc,
        page_id: &str,
    ) -> Result<Option<TranscriptPage>, DocError> {
        let Some(slot) = self.slots.iter().find(|slot| slot.descriptor.id == page_id) else {
            return Ok(None);
        };
        Ok(Some(page_from_slot(doc, slot)?))
    }

    pub fn bootstrap(
        &self,
        doc: &SessionDoc,
        sequence: u64,
    ) -> Result<TranscriptBootstrap, DocError> {
        let mut selected = Vec::new();
        let mut count = 0usize;
        for slot in self.slots.iter().rev() {
            selected.push(page_from_slot(doc, slot)?);
            count += slot.descriptor.message_count;
            if count >= TRANSCRIPT_BOOTSTRAP_MESSAGE_COUNT {
                break;
            }
        }
        selected.reverse();
        Ok(TranscriptBootstrap {
            sequence,
            manifest: self.manifest.clone(),
            pages: selected,
        })
    }

    pub fn live_page(&self, doc: &SessionDoc) -> Result<Option<TranscriptPage>, DocError> {
        self.slots
            .last()
            .map(|slot| page_from_slot(doc, slot))
            .transpose()
    }

    /// Search all rendered message content, newest first, without materializing
    /// transcript pages in the client.
    pub fn search(
        &self,
        doc: &SessionDoc,
        query: &str,
        limit: usize,
    ) -> Result<Vec<TranscriptSearchResult>, DocError> {
        let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        if terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let mut results = Vec::new();
        for slot in self.slots.iter().rev() {
            let entries = read_slot(doc, slot)?;
            for (offset, entry) in entries.iter().enumerate().rev() {
                let text = searchable_text(entry);
                let lowercase = text.to_lowercase();
                if !terms.iter().all(|term| lowercase.contains(term)) {
                    continue;
                }
                results.push(TranscriptSearchResult {
                    message_id: entry.id.clone(),
                    page_id: slot.descriptor.id.clone(),
                    ordinal: slot.descriptor.first_ordinal + offset,
                    role: entry.role,
                    preview: search_preview(&text, &terms, 240),
                    created_at: entry.created_at,
                });
                if results.len() == limit {
                    return Ok(results);
                }
            }
        }
        Ok(results)
    }

    pub fn physical_len(&self) -> usize {
        self.slots.last().map_or(0, |slot| slot.physical_end)
    }
}

fn append_joined(entries: &mut Vec<SessionMessageEntry>, entry: SessionMessageEntry) {
    if let Some(root_id) = entry.continuation_of.as_deref()
        && let Some(root) = entries.iter_mut().find(|candidate| candidate.id == root_id)
    {
        root.parts.extend(entry.parts);
    } else {
        entries.push(entry);
    }
}

fn push_slot(
    slots: &mut Vec<PageSlot>,
    physical_start: usize,
    physical_end: usize,
    first_ordinal: usize,
    messages: &[SessionMessageEntry],
) {
    let id = messages
        .first()
        .map_or_else(|| format!("page-{first_ordinal}"), |entry| entry.id.clone());
    slots.push(PageSlot {
        descriptor: TranscriptPageDescriptor {
            id,
            revision: page_revision(messages),
            first_ordinal,
            message_count: messages.len(),
            estimated_bytes: page_bytes(messages),
            previous_page_id: None,
            next_page_id: None,
            live: false,
        },
        physical_start,
        physical_end,
    });
}

fn page_from_slot(doc: &SessionDoc, slot: &PageSlot) -> Result<TranscriptPage, DocError> {
    let messages = read_slot(doc, slot)?;
    Ok(TranscriptPage {
        id: slot.descriptor.id.clone(),
        revision: page_revision(&messages),
        first_ordinal: slot.descriptor.first_ordinal,
        messages,
    })
}

fn read_slot(doc: &SessionDoc, slot: &PageSlot) -> Result<Vec<SessionMessageEntry>, DocError> {
    let mut entries = Vec::with_capacity(slot.descriptor.message_count);
    for index in slot.physical_start..slot.physical_end {
        if let Some(entry) = doc.read_entry_at(index)? {
            entries.push(entry);
        }
    }
    Ok(join_continuation_entries(entries))
}

fn page_bytes(messages: &[SessionMessageEntry]) -> usize {
    messages.iter().map(message_bytes).sum()
}

fn message_bytes(entry: &SessionMessageEntry) -> usize {
    entry.id.len()
        + entry.device_id.len()
        + entry.continuation_of.as_ref().map_or(0, String::len)
        + entry.parts.iter().map(MessagePart::byte_len).sum::<usize>()
        + 48
}

fn page_revision(messages: &[SessionMessageEntry]) -> String {
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
                hash.write_u64(message_bytes(entry) as u64);
            }
        }
    }
    hash.finish_hex()
}

fn preview(entry: &SessionMessageEntry, limit: usize) -> String {
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

fn searchable_text(entry: &SessionMessageEntry) -> String {
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

fn search_preview(text: &str, terms: &[String], limit: usize) -> String {
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

#[derive(Debug, Clone, Copy)]
struct Hash64(u64);

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
    use crate::{MessageStatus, schema::MessageRole};

    fn entry(id: &str, role: MessageRole, text: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.into(),
            }],
            created_at: 1,
            device_id: "d".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        }
    }

    #[test]
    fn catalog_pages_and_bootstrap_cover_the_tail() {
        let doc = SessionDoc::init("c").unwrap();
        for index in 0..90 {
            doc.push_message(&entry(
                &format!("m{index}"),
                if index % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                "hello",
            ))
            .unwrap();
        }
        let catalog = TranscriptCatalog::build(&doc).unwrap();
        assert_eq!(catalog.manifest().total_messages, 90);
        assert_eq!(catalog.manifest().pages.len(), 3);
        assert_eq!(catalog.manifest().turns.len(), 45);
        let bootstrap = catalog.bootstrap(&doc, 7).unwrap();
        assert_eq!(bootstrap.sequence, 7);
        assert!(
            bootstrap
                .pages
                .iter()
                .map(|page| page.messages.len())
                .sum::<usize>()
                >= 64
        );
        assert_eq!(
            bootstrap.pages.last().unwrap().messages.last().unwrap().id,
            "m89"
        );
    }

    #[test]
    fn continuation_stays_with_its_root() {
        let doc = SessionDoc::init("c").unwrap();
        doc.push_message(&entry("root", MessageRole::Assistant, "a"))
            .unwrap();
        let mut continuation = entry("root-c1", MessageRole::Assistant, "b");
        continuation.continuation_of = Some("root".into());
        doc.push_message(&continuation).unwrap();
        let catalog = TranscriptCatalog::build(&doc).unwrap();
        let page = catalog.live_page(&doc).unwrap().unwrap();
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].parts.len(), 2);
    }

    #[test]
    fn search_covers_cold_pages_and_returns_page_anchors() {
        let doc = SessionDoc::init("c").unwrap();
        for index in 0..70 {
            let text = if index == 3 || index == 68 {
                "before the distinctive Needle phrase after"
            } else {
                "ordinary message"
            };
            let mut message = entry(&format!("m{index}"), MessageRole::Assistant, text);
            message.created_at = index as i64;
            doc.push_message(&message).unwrap();
        }

        let catalog = TranscriptCatalog::build(&doc).unwrap();
        let results = catalog.search(&doc, "needle PHRASE", 20).unwrap();
        assert_eq!(
            results
                .iter()
                .map(|result| result.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["m68", "m3"]
        );
        assert_eq!(results[0].ordinal, 68);
        assert_eq!(results[0].created_at, 68);
        assert!(
            catalog
                .manifest()
                .pages
                .iter()
                .any(|page| page.id == results[1].page_id)
        );
        assert!(results[0].preview.contains("distinctive Needle phrase"));
    }

    #[test]
    fn search_preview_keeps_the_match_near_the_front() {
        let text = "one two three four five six seven eight nine distinctive Needle phrase after";
        let preview = search_preview(text, &["needle".into()], 240);
        assert!(preview.starts_with("… eight nine distinctive Needle"));
    }
}
